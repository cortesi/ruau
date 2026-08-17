//! The engine's cancellation signal.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    task::{Context, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

/// The foreign cancellation primitive [`Cancel::new`] consumes, re-exported so
/// embedders can construct and link tokens without adding a direct
/// `tokio-util` dependency (and without risking a version mismatch with the
/// one the engine was built against).
pub use tokio_util::sync::CancellationToken;
use tokio_util::sync::WaitForCancellationFutureOwned;

/// A cancellation signal the engine reads at its safepoints. It is a thin wrapper
/// over the cancellation primitive so the **synchronous engine core depends on
/// `Cancel`, not on Tokio directly**: the safepoint check is
/// `Cancel::is_cancelled`.
///
/// `Option<Cancel>` is the "configured or not" state: `None` is an uncancellable
/// run, and a `Limits` overlay inherits an unset `cancel` from its base.
#[derive(Clone, Debug)]
pub struct Cancel {
    state: Arc<CancelState>,
    watchdog_release: Option<Arc<WatchdogRelease>>,
}

/// First cause that stopped a VM invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// The caller requested cancellation.
    Cancelled,
    /// A wall-clock deadline elapsed.
    Deadline,
}

#[derive(Debug)]
struct CancelState {
    token: CancellationToken,
    stop: AtomicU64,
    parent: Option<Arc<Self>>,
}

static NEXT_STOP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

impl CancelState {
    fn record(&self, reason: StopReason) {
        let sequence = NEXT_STOP_SEQUENCE.fetch_add(1, Ordering::Relaxed).max(1);
        let code = match reason {
            StopReason::Cancelled => 1,
            StopReason::Deadline => 2,
        };
        let encoded = sequence.saturating_mul(4) | code;
        let _result = self
            .stop
            .compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire);
    }

    fn recorded_reason(&self) -> Option<(u64, StopReason)> {
        let encoded = self.stop.load(Ordering::Acquire);
        let own = (encoded != 0).then(|| {
            let reason = if encoded & 3 == 2 {
                StopReason::Deadline
            } else {
                StopReason::Cancelled
            };
            (encoded >> 2, reason)
        });
        let parent = self
            .parent
            .as_ref()
            .and_then(|parent| parent.recorded_reason());
        match (own, parent) {
            (Some(own), Some(parent)) => Some(if own.0 <= parent.0 { own } else { parent }),
            (Some(reason), None) | (None, Some(reason)) => Some(reason),
            (None, None) if self.token.is_cancelled() => Some((u64::MAX, StopReason::Cancelled)),
            (None, None) => None,
        }
    }
}

static WATCHDOG_TIMER: OnceLock<Result<WatchdogTimer, String>> = OnceLock::new();

/// A runtime-free bridge from [`Cancel`] to the atomic flag consumed by
/// synchronous compiler and type-checker safepoints.
///
/// The bridge registers a waker directly with the cancellation token. It does
/// not spawn a polling thread. Keep this value alive for as long as consumers
/// may read the [`Arc<AtomicBool>`] returned by [`Self::flag`].
pub struct CancellationFlag {
    flag: Arc<AtomicBool>,
    _registration: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl CancellationFlag {
    fn new(token: CancellationToken) -> Self {
        let flag = Arc::new(AtomicBool::new(token.is_cancelled()));
        let mut registration = Box::pin(token.cancelled_owned());
        let waker = Waker::from(Arc::new(CancellationFlagWaker {
            flag: Arc::clone(&flag),
        }));
        let mut context = Context::from_waker(&waker);
        if registration.as_mut().poll(&mut context).is_ready() {
            flag.store(true, Ordering::Relaxed);
        }
        Self {
            flag,
            _registration: registration,
        }
    }

    /// Returns the shared atomic flag set when the source token is cancelled.
    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    /// Whether cancellation has reached this bridge.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for CancellationFlag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationFlag")
            .field("is_cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

struct CancellationFlagWaker {
    flag: Arc<AtomicBool>,
}

impl Wake for CancellationFlagWaker {
    fn wake(self: Arc<Self>) {
        self.flag.store(true, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct WatchdogRelease {
    _guard: WatchdogGuard,
    _parent: Option<Arc<Self>>,
}

#[derive(Debug)]
struct WatchdogTimer {
    sender: mpsc::Sender<WatchdogCommand>,
    next_id: AtomicU64,
}

impl WatchdogTimer {
    fn try_new() -> io::Result<Self> {
        Self::try_new_with(|timer| {
            thread::Builder::new()
                .name("ruau-cancel-timer".to_owned())
                .spawn(timer)
        })
    }

    fn try_new_with(
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<thread::JoinHandle<()>>,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        spawn(Box::new(move || run_watchdog_timer(&receiver)))?;
        Ok(Self {
            sender,
            next_id: AtomicU64::new(1),
        })
    }

    fn arm(&self, state: &Arc<CancelState>, timeout: Duration) -> Result<WatchdogGuard, ()> {
        let deadline = Instant::now().checked_add(timeout).ok_or(())?;
        if deadline <= Instant::now() {
            return Err(());
        };

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.sender
            .send(WatchdogCommand::Arm {
                id,
                deadline,
                state: Arc::clone(state),
            })
            .map_err(|_| ())?;
        Ok(WatchdogGuard {
            id,
            sender: self.sender.clone(),
        })
    }
}

#[derive(Debug)]
struct WatchdogGuard {
    id: u64,
    sender: mpsc::Sender<WatchdogCommand>,
}

impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        drop(self.sender.send(WatchdogCommand::Disarm(self.id)));
    }
}

enum WatchdogCommand {
    Arm {
        id: u64,
        deadline: Instant,
        state: Arc<CancelState>,
    },
    Disarm(u64),
}

impl Cancel {
    /// Wraps a cancellation token as the engine's cancellation signal.
    #[must_use]
    pub fn new(token: CancellationToken) -> Self {
        Self {
            state: Arc::new(CancelState {
                token,
                stop: AtomicU64::new(0),
                parent: None,
            }),
            watchdog_release: None,
        }
    }

    /// A fresh, manually-triggered signal: call [`Cancel::cancel`] to fire it.
    /// `CancellationToken` is runtime-free, so this works in fully synchronous
    /// embeddings with no Tokio runtime.
    #[must_use]
    pub fn manual() -> Self {
        Self::new(CancellationToken::new())
    }

    /// Requests cancellation; every clone of this signal observes it at the
    /// next dispatch safepoint.
    pub fn cancel(&self) {
        self.stop(StopReason::Cancelled);
    }

    /// Stops this signal with an explicit first-writer-wins cause.
    #[doc(hidden)]
    pub fn stop(&self, reason: StopReason) {
        self.state.record(reason);
        self.state.token.cancel();
    }

    /// A wall-clock watchdog for synchronous VM calls: the returned signal
    /// fires after `timeout` on a process-wide shared timer thread. The
    /// synchronous engine otherwise enforces only gas and logical
    /// deadlines — without this (or an externally cancelled signal), a
    /// `Deadline::Wall` is only honored by the async driver's governed await.
    #[must_use]
    pub fn after(timeout: Duration) -> Self {
        Self::with_timeout(
            CancellationToken::new(),
            timeout,
            None,
            None,
            watchdog_timer(),
        )
    }

    /// Whether cancellation has been requested. The synchronous safepoint check.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.token.is_cancelled()
    }

    /// Returns the first known cause after this signal stops.
    #[must_use]
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.state.recorded_reason().map(|(_, reason)| reason)
    }

    /// Resolves when cancellation is requested. Used by the async driver's
    /// governed await and the runner's stage selects.
    pub async fn cancelled(&self) {
        self.state.token.cancelled().await;
    }

    /// Bridges this signal into an atomic flag for synchronous cooperative
    /// cancellation APIs without allocating an OS thread.
    #[must_use]
    pub fn atomic_flag(&self) -> CancellationFlag {
        CancellationFlag::new(self.state.token.clone())
    }

    /// A child signal: cancelling `self` cancels the child, while cancelling
    /// the child leaves `self` untouched. The runner scopes each execution
    /// stage with one.
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            state: Arc::new(CancelState {
                token: self.state.token.child_token(),
                stop: AtomicU64::new(0),
                parent: Some(Arc::clone(&self.state)),
            }),
            watchdog_release: self.watchdog_release.clone(),
        }
    }

    /// A timed child signal: cancelling `self` cancels the child, while the
    /// timeout or cancelling the child leaves `self` untouched.
    #[must_use]
    pub fn child_after(&self, timeout: Duration) -> Self {
        Self::with_timeout(
            self.state.token.child_token(),
            timeout,
            Some(Arc::clone(&self.state)),
            self.watchdog_release.clone(),
            watchdog_timer(),
        )
    }

    fn with_timeout(
        token: CancellationToken,
        timeout: Duration,
        parent_state: Option<Arc<CancelState>>,
        parent_watchdog: Option<Arc<WatchdogRelease>>,
        timer: Option<&WatchdogTimer>,
    ) -> Self {
        let state = Arc::new(CancelState {
            token,
            stop: AtomicU64::new(0),
            parent: parent_state,
        });
        let watchdog_release = match timer.and_then(|timer| timer.arm(&state, timeout).ok()) {
            Some(guard) => Some(Arc::new(WatchdogRelease {
                _guard: guard,
                _parent: parent_watchdog,
            })),
            None => {
                // A wall-clock limit must never become unbounded because the
                // scheduler is unavailable or the deadline cannot be represented.
                state.record(StopReason::Deadline);
                state.token.cancel();
                parent_watchdog
            }
        };
        Self {
            state,
            watchdog_release,
        }
    }
}

fn watchdog_timer() -> Option<&'static WatchdogTimer> {
    WATCHDOG_TIMER
        .get_or_init(|| WatchdogTimer::try_new().map_err(|error| error.to_string()))
        .as_ref()
        .ok()
}

fn run_watchdog_timer(receiver: &mpsc::Receiver<WatchdogCommand>) {
    let mut deadlines = BTreeMap::<(Instant, u64), Arc<CancelState>>::new();
    let mut ids = HashMap::<u64, Instant>::new();
    loop {
        cancel_expired_watchdogs(&mut deadlines, &mut ids);
        let command = match deadlines.first_key_value() {
            Some((&(deadline, _), _)) => {
                receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match command {
            Ok(WatchdogCommand::Arm {
                id,
                deadline,
                state,
            }) => {
                if deadline <= Instant::now() {
                    state.record(StopReason::Deadline);
                    state.token.cancel();
                    continue;
                }
                if let Some(previous) = ids.insert(id, deadline) {
                    deadlines.remove(&(previous, id));
                }
                deadlines.insert((deadline, id), state);
            }
            Ok(WatchdogCommand::Disarm(id)) => {
                if let Some(deadline) = ids.remove(&id) {
                    deadlines.remove(&(deadline, id));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                for state in deadlines.into_values() {
                    state.record(StopReason::Deadline);
                    state.token.cancel();
                }
                break;
            }
        }
    }
}

fn cancel_expired_watchdogs(
    deadlines: &mut BTreeMap<(Instant, u64), Arc<CancelState>>,
    ids: &mut HashMap<u64, Instant>,
) {
    let now = Instant::now();
    while deadlines
        .first_key_value()
        .is_some_and(|(&(deadline, _), _)| deadline <= now)
    {
        let ((_, id), state) = deadlines.pop_first().expect("first entry exists");
        ids.remove(&id);
        state.record(StopReason::Deadline);
        state.token.cancel();
    }
}

#[cfg(any())]
mod tests {
    use std::{thread, time::Duration};

    use tokio_util::sync::CancellationToken;

    use super::{Cancel, StopReason, WatchdogTimer};

    #[test]
    fn is_cancelled_reflects_the_backing_token() {
        let token = CancellationToken::new();
        let cancel = Cancel::new(token.clone());
        assert!(!cancel.is_cancelled());
        token.cancel();
        assert!(cancel.is_cancelled());
        assert_eq!(cancel.stop_reason(), Some(StopReason::Cancelled));
    }

    #[test]
    fn watchdog_scheduler_failure_cancels_immediately() {
        let token = CancellationToken::new();
        let cancel = Cancel::with_timeout(token.clone(), Duration::from_secs(60), None, None, None);
        assert!(token.is_cancelled());
        assert!(cancel.is_cancelled());
        assert_eq!(cancel.stop_reason(), Some(StopReason::Deadline));
    }

    #[test]
    fn atomic_flag_tracks_external_token_cancellation_without_polling() {
        let token = CancellationToken::new();
        let cancel = Cancel::new(token.clone());
        let flag = cancel.atomic_flag();

        assert!(!flag.is_cancelled());
        token.cancel();
        assert!(flag.is_cancelled());
        assert!(flag.flag().load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn atomic_flag_starts_cancelled_when_token_already_fired() {
        let cancel = Cancel::manual();
        cancel.cancel();

        assert!(cancel.atomic_flag().is_cancelled());
    }

    #[test]
    fn one_watchdog_scheduler_services_many_signals() {
        let timer = WatchdogTimer::try_new().expect("watchdog timer starts");
        let cancels = (0..128)
            .map(|_| {
                Cancel::with_timeout(
                    CancellationToken::new(),
                    Duration::from_millis(5),
                    None,
                    None,
                    Some(&timer),
                )
            })
            .collect::<Vec<_>>();

        for _ in 0..100 {
            if cancels.iter().all(Cancel::is_cancelled) {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("shared watchdog timer did not cancel every signal");
    }

    #[test]
    fn dropping_the_last_timed_signal_disarms_its_deadline() {
        let timer = WatchdogTimer::try_new().expect("watchdog timer starts");
        let token = CancellationToken::new();
        let cancel = Cancel::with_timeout(
            token.clone(),
            Duration::from_millis(100),
            None,
            None,
            Some(&timer),
        );

        drop(cancel);
        thread::sleep(Duration::from_millis(150));
        assert!(!token.is_cancelled());
    }

    #[test]
    fn child_keeps_a_parent_watchdog_armed() {
        let parent = Cancel::after(Duration::from_millis(1));
        let child = parent.child();
        drop(parent);

        for _ in 0..100 {
            if child.is_cancelled() {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("child did not retain its parent's watchdog");
    }

    #[test]
    fn timed_child_composes_parent_and_timeout_cancellation() {
        let parent = Cancel::manual();
        let child = parent.child_after(Duration::from_secs(60));
        parent.cancel();
        assert!(child.is_cancelled());
        assert_eq!(child.stop_reason(), Some(StopReason::Cancelled));

        let parent = Cancel::manual();
        let child = parent.child_after(Duration::from_millis(1));
        for _ in 0..100 {
            if child.is_cancelled() {
                assert!(!parent.is_cancelled());
                assert_eq!(child.stop_reason(), Some(StopReason::Deadline));
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("timed child did not observe its deadline");
    }

    #[test]
    fn explicit_stop_reason_is_first_writer_wins() {
        let deadline_first = Cancel::manual();
        deadline_first.stop(StopReason::Deadline);
        deadline_first.cancel();
        assert_eq!(deadline_first.stop_reason(), Some(StopReason::Deadline));

        let cancellation_first = Cancel::manual();
        cancellation_first.cancel();
        cancellation_first.stop(StopReason::Deadline);
        assert_eq!(
            cancellation_first.stop_reason(),
            Some(StopReason::Cancelled)
        );
    }
}
