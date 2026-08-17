//! Bounded scheduling primitives for retained script invocations.
//!
//! [`InvocationService`] admits work into fair logical lanes and runs it on a
//! fixed pool of worker threads. A coordinator thread owns admission,
//! coalescing, cancellation, and discard delivery.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, mpsc},
    thread,
};

use ruau_vm::Cancel;

/// Maximum number of live logical lanes in one scheduler.
pub const MAX_LOGICAL_LANES: usize = 256;
/// Maximum number of queued invocations in one logical lane.
pub const MAX_PENDING_PER_LANE: usize = 64;
/// Fixed number of operating-system workers used by one invocation service.
pub const INVOCATION_WORKER_THREADS: usize = 4;

/// Stable identity for one invocation owner.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InvocationOwner(u64);

impl InvocationOwner {
    /// Create an owner identity from an app-managed stable value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the app-managed value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Stable identity for one logical FIFO lane.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InvocationLane(u64);

impl InvocationLane {
    /// Create a lane identity from an app-managed stable value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the app-managed value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Process-local identity for one admitted invocation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InvocationTicketId(u64);

impl InvocationTicketId {
    /// Return the process-local numeric identity.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// A ticket whose type records the expected invocation result.
#[derive(Debug, Eq, Hash, PartialEq)]
pub struct InvocationTicket<R> {
    id: InvocationTicketId,
    result: PhantomData<fn() -> R>,
}

impl<R> InvocationTicket<R> {
    /// Return the untyped ticket identity used by scheduler events.
    #[must_use]
    pub const fn id(self) -> InvocationTicketId {
        self.id
    }
}

impl<R> Clone for InvocationTicket<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for InvocationTicket<R> {}

/// Admission class for an invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvocationClass {
    /// A non-repeat invocation that must not coalesce.
    Initial,
    /// A repeat invocation that can coalesce by a stable key.
    Repeat {
        /// Embedder-defined coalescing key.
        coalesce_key: String,
    },
}

impl InvocationClass {
    /// Create an initial invocation class.
    #[must_use]
    pub const fn initial() -> Self {
        Self::Initial
    }

    /// Create a repeat invocation class.
    #[must_use]
    pub fn repeat(coalesce_key: impl Into<String>) -> Self {
        Self::Repeat {
            coalesce_key: coalesce_key.into(),
        }
    }

    fn repeat_coalesce_key(&self) -> Option<&str> {
        match self {
            Self::Initial => None,
            Self::Repeat { coalesce_key } => Some(coalesce_key),
        }
    }
}

/// Successful scheduler admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvocationAdmission<R> {
    /// The scheduler added a new invocation.
    Queued(InvocationTicket<R>),
    /// The scheduler merged this repeat into an existing invocation.
    Coalesced(InvocationTicket<R>),
}

impl<R> InvocationAdmission<R> {
    /// Return the ticket for the admitted or coalesced invocation.
    #[must_use]
    pub const fn ticket(self) -> InvocationTicket<R> {
        match self {
            Self::Queued(ticket) | Self::Coalesced(ticket) => ticket,
        }
    }
}

/// Scheduler admission failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvocationAdmissionError {
    /// The scheduler service has stopped and cannot accept work.
    ServiceUnavailable {
        /// Rejected lane.
        lane: InvocationLane,
        /// Rejected invocation label.
        label: String,
    },
    /// The scheduler has no slot for another logical lane.
    LaneLimit {
        /// Rejected lane.
        lane: InvocationLane,
        /// Rejected invocation label.
        label: String,
        /// Configured lane limit.
        limit: usize,
    },
    /// The target lane has no queued slot and no repeat to evict.
    LaneOverflow {
        /// Rejected lane.
        lane: InvocationLane,
        /// Rejected invocation label.
        label: String,
        /// Configured pending limit.
        limit: usize,
    },
    /// The lane is active for a different stable owner.
    LaneOwnerMismatch {
        /// Rejected lane.
        lane: InvocationLane,
        /// Rejected invocation label.
        label: String,
        /// Owner that already controls the lane.
        current: InvocationOwner,
        /// Owner that submitted the rejected invocation.
        submitted: InvocationOwner,
    },
}

impl fmt::Display for InvocationAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServiceUnavailable { lane, label } => write!(
                formatter,
                "invocation {label:?} rejected: scheduler for lane {} is unavailable",
                lane.value()
            ),
            Self::LaneLimit { lane, label, limit } => write!(
                formatter,
                "invocation {label:?} rejected: lane {} exceeds the {limit}-lane limit",
                lane.value()
            ),
            Self::LaneOverflow { lane, label, limit } => write!(
                formatter,
                "invocation {label:?} rejected: lane {} has {limit} pending invocations",
                lane.value()
            ),
            Self::LaneOwnerMismatch {
                lane,
                label,
                current,
                submitted,
            } => write!(
                formatter,
                "invocation {label:?} rejected: lane {} belongs to owner {}, not owner {}",
                lane.value(),
                current.value(),
                submitted.value()
            ),
        }
    }
}

impl std::error::Error for InvocationAdmissionError {}

/// Reason that the scheduler discarded an invocation without a result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvocationDiscardReason {
    /// A newer admission evicted a pending repeat.
    RepeatEvicted,
    /// The invocation cancellation signal fired before delivery.
    Cancelled,
}

/// Counts from one owner or lane cancellation request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InvocationCancellation {
    /// Queued invocations removed immediately.
    pub queued: usize,
    /// Active invocation segments that received a cancellation signal.
    pub active: usize,
}

/// Result delivery policy after one invocation completes.
#[derive(Debug, Eq, PartialEq)]
pub enum InvocationCompletion<R> {
    /// Deliver the result to the ticket owner.
    Deliver(R),
    /// Cancellation made the ticket owner stale and the scheduler dropped the result.
    Discarded,
}

type ServiceWork = Box<dyn FnOnce(Cancel) + Send + 'static>;
type DiscardWork = Box<dyn FnOnce(InvocationDiscardReason) + Send + 'static>;

struct ServiceJob {
    ticket: InvocationTicketId,
    owner: InvocationOwner,
    lane: InvocationLane,
    label: String,
    class: InvocationClass,
    cancel: Cancel,
    run: ServiceWork,
    discard: DiscardWork,
}

struct ServiceLane {
    owner: InvocationOwner,
    queued: VecDeque<ServiceJob>,
    active: Option<(InvocationTicketId, Cancel)>,
    ready: bool,
}

impl ServiceLane {
    fn new(owner: InvocationOwner) -> Self {
        Self {
            owner,
            queued: VecDeque::new(),
            active: None,
            ready: false,
        }
    }
}

enum ServiceCommand {
    Submit {
        owner: InvocationOwner,
        lane: InvocationLane,
        label: String,
        class: InvocationClass,
        cancel: Cancel,
        run: ServiceWork,
        discard: DiscardWork,
        reply: mpsc::SyncSender<Result<InvocationAdmission<()>, InvocationAdmissionError>>,
    },
    Complete {
        lane: InvocationLane,
        ticket: InvocationTicketId,
    },
    CancelOwner {
        owner: InvocationOwner,
        reply: mpsc::SyncSender<InvocationCancellation>,
    },
    CancelLane {
        lane: InvocationLane,
        reply: mpsc::SyncSender<InvocationCancellation>,
    },
    CancelAll {
        reply: mpsc::SyncSender<InvocationCancellation>,
    },
    Shutdown,
}

struct InvocationServiceHandle {
    commands: mpsc::Sender<ServiceCommand>,
}

impl Drop for InvocationServiceHandle {
    fn drop(&mut self) {
        let _send_result = self.commands.send(ServiceCommand::Shutdown);
    }
}

/// A fixed-worker, bounded FIFO service shared by GUI script entry points.
///
/// Jobs in one lane execute in submission order. Jobs in other lanes can use
/// another fixed worker while a retained invocation waits on host work. The
/// Ruau runtime still serializes individual VM segments through its own lock.
#[derive(Clone)]
pub struct InvocationService {
    handle: Arc<InvocationServiceHandle>,
}

impl fmt::Debug for InvocationService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InvocationService")
            .finish_non_exhaustive()
    }
}

impl Default for InvocationService {
    fn default() -> Self {
        Self::new()
    }
}

impl InvocationService {
    /// Start one invocation service with fixed production limits.
    #[must_use]
    pub fn new() -> Self {
        Self::with_thread_name_prefix("ruau")
    }

    /// Start one invocation service with an embedder-specific thread-name prefix.
    #[must_use]
    pub fn with_thread_name_prefix(prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        Self::with_limits(
            &prefix,
            INVOCATION_WORKER_THREADS,
            MAX_LOGICAL_LANES,
            MAX_PENDING_PER_LANE,
        )
    }

    /// Starts an invocation service with explicit worker and queue limits.
    ///
    /// # Panics
    /// Panics when `worker_count` is zero or a worker thread cannot start.
    #[must_use]
    pub fn with_limits(
        thread_name_prefix: &str,
        worker_count: usize,
        max_lanes: usize,
        max_pending_per_lane: usize,
    ) -> Self {
        assert!(worker_count > 0, "invocation service needs a worker");
        let (command_tx, command_rx) = mpsc::channel();
        let (worker_tx, worker_rx) = mpsc::channel::<ServiceJob>();
        let worker_rx = Arc::new(std::sync::Mutex::new(worker_rx));
        for index in 0..worker_count {
            let jobs = Arc::clone(&worker_rx);
            let completed = command_tx.clone();
            let worker_name = format!("{thread_name_prefix}-invocation-{index}");
            thread::Builder::new()
                .name(worker_name)
                .spawn(move || service_worker(&jobs, &completed))
                .expect("invocation worker thread must start");
        }
        let coordinator_tx = command_tx;
        let coordinator_name = format!("{thread_name_prefix}-invocation-scheduler");
        thread::Builder::new()
            .name(coordinator_name)
            .spawn(move || {
                service_coordinator(
                    &command_rx,
                    &worker_tx,
                    worker_count,
                    max_lanes,
                    max_pending_per_lane,
                );
            })
            .expect("invocation scheduler thread must start");
        Self {
            handle: Arc::new(InvocationServiceHandle {
                commands: coordinator_tx,
            }),
        }
    }

    /// Submit typed work to one logical lane.
    ///
    /// The returned task receives `Discarded` when queued work is replaced or
    /// cancelled. Active work receives its cancellation signal and its result
    /// is discarded if the signal fires before delivery.
    pub fn submit<R: Send + 'static>(
        &self,
        owner: InvocationOwner,
        lane: InvocationLane,
        label: impl Into<String>,
        class: InvocationClass,
        work: impl FnOnce(Cancel) -> R + Send + 'static,
    ) -> Result<InvocationTask<R>, InvocationAdmissionError> {
        self.submit_with_cancel(owner, lane, label, class, Cancel::manual(), work)
    }

    /// Submit typed work with an existing owner cancellation signal.
    pub fn submit_with_cancel<R: Send + 'static>(
        &self,
        owner: InvocationOwner,
        lane: InvocationLane,
        label: impl Into<String>,
        class: InvocationClass,
        cancel: Cancel,
        work: impl FnOnce(Cancel) -> R + Send + 'static,
    ) -> Result<InvocationTask<R>, InvocationAdmissionError> {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let deliver = result_tx.clone();
        let run = Box::new(move |cancel: Cancel| {
            let result = work(cancel.clone());
            let completion = if cancel.is_cancelled() {
                InvocationCompletion::Discarded
            } else {
                InvocationCompletion::Deliver(result)
            };
            let _send_result = deliver.send(completion);
        });
        let discard = Box::new(move |_reason| {
            let _send_result = result_tx.send(InvocationCompletion::Discarded);
        });
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let label = label.into();
        self.handle
            .commands
            .send(ServiceCommand::Submit {
                owner,
                lane,
                label: label.clone(),
                class,
                cancel,
                run,
                discard,
                reply: reply_tx,
            })
            .map_err(|_| InvocationAdmissionError::ServiceUnavailable {
                lane,
                label: label.clone(),
            })?;
        let admission = reply_rx
            .recv()
            .map_err(|_| InvocationAdmissionError::ServiceUnavailable { lane, label })??;
        Ok(InvocationTask {
            ticket: InvocationTicket {
                id: admission.ticket().id(),
                result: PhantomData,
            },
            receiver: result_rx,
        })
    }

    /// Cancel queued and active work for one stable owner.
    #[must_use]
    pub fn cancel_owner(&self, owner: InvocationOwner) -> InvocationCancellation {
        let (reply, receiver) = mpsc::sync_channel(1);
        if self
            .handle
            .commands
            .send(ServiceCommand::CancelOwner { owner, reply })
            .is_err()
        {
            return InvocationCancellation::default();
        }
        receiver.recv().unwrap_or_default()
    }

    /// Cancel queued and active work in one logical lane.
    #[must_use]
    pub fn cancel_lane(&self, lane: InvocationLane) -> InvocationCancellation {
        let (reply, receiver) = mpsc::sync_channel(1);
        if self
            .handle
            .commands
            .send(ServiceCommand::CancelLane { lane, reply })
            .is_err()
        {
            return InvocationCancellation::default();
        }
        receiver.recv().unwrap_or_default()
    }

    /// Cancel every queued and active invocation in this service.
    #[must_use]
    pub fn cancel_all(&self) -> InvocationCancellation {
        let (reply, receiver) = mpsc::sync_channel(1);
        if self
            .handle
            .commands
            .send(ServiceCommand::CancelAll { reply })
            .is_err()
        {
            return InvocationCancellation::default();
        }
        receiver.recv().unwrap_or_default()
    }
}

/// A typed result receiver for one admitted invocation.
pub struct InvocationTask<R> {
    ticket: InvocationTicket<R>,
    receiver: mpsc::Receiver<InvocationCompletion<R>>,
}

impl<R> InvocationTask<R> {
    /// Return this task's process-local ticket.
    #[must_use]
    pub const fn ticket(&self) -> InvocationTicket<R> {
        self.ticket
    }

    /// Wait for delivery or stale-owner discard.
    ///
    /// # Errors
    ///
    /// Returns a receive error only if the service terminates unexpectedly.
    pub fn recv(self) -> Result<InvocationCompletion<R>, mpsc::RecvError> {
        self.receiver.recv()
    }

    /// Poll for delivery without blocking.
    ///
    /// # Errors
    ///
    /// Returns the standard channel empty or disconnected state.
    pub fn try_recv(&self) -> Result<InvocationCompletion<R>, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

fn service_worker(
    jobs: &Arc<std::sync::Mutex<mpsc::Receiver<ServiceJob>>>,
    completed: &mpsc::Sender<ServiceCommand>,
) {
    loop {
        let job = {
            let receiver = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            receiver.recv()
        };
        let Ok(job) = job else {
            break;
        };
        let ServiceJob {
            ticket,
            lane,
            label: _,
            cancel,
            run,
            ..
        } = job;
        drop(catch_unwind(AssertUnwindSafe(|| run(cancel))));
        if completed
            .send(ServiceCommand::Complete { lane, ticket })
            .is_err()
        {
            break;
        }
    }
}

fn service_coordinator(
    commands: &mpsc::Receiver<ServiceCommand>,
    workers: &mpsc::Sender<ServiceJob>,
    worker_count: usize,
    max_lanes: usize,
    max_pending_per_lane: usize,
) {
    let mut lanes = HashMap::<InvocationLane, ServiceLane>::new();
    let mut ready = VecDeque::<InvocationLane>::new();
    let mut next_ticket = 1_u64;
    let mut active_workers = 0_usize;
    while let Ok(command) = commands.recv() {
        match command {
            ServiceCommand::Submit {
                owner,
                lane,
                label,
                class,
                cancel,
                run,
                discard,
                reply,
            } => {
                let result = admit_service_job(
                    &mut lanes,
                    &mut ready,
                    &mut next_ticket,
                    max_lanes,
                    max_pending_per_lane,
                    owner,
                    lane,
                    label,
                    class,
                    cancel,
                    run,
                    discard,
                );
                let _send_result = reply.send(result);
            }
            ServiceCommand::Complete { lane, ticket } => {
                active_workers = active_workers.saturating_sub(1);
                if let Some(state) = lanes.get_mut(&lane) {
                    debug_assert_eq!(state.active.as_ref().map(|active| active.0), Some(ticket));
                    state.active = None;
                    schedule_service_lane(&mut ready, lane, state);
                }
                remove_empty_service_lane(&mut lanes, lane);
            }
            ServiceCommand::CancelOwner { owner, reply } => {
                let cancellation =
                    cancel_service_jobs(&mut lanes, &mut ready, |job_owner, _| job_owner == owner);
                let _send_result = reply.send(cancellation);
            }
            ServiceCommand::CancelLane { lane, reply } => {
                let cancellation =
                    cancel_service_jobs(&mut lanes, &mut ready, |_, job_lane| job_lane == lane);
                let _send_result = reply.send(cancellation);
            }
            ServiceCommand::CancelAll { reply } => {
                let cancellation = cancel_service_jobs(&mut lanes, &mut ready, |_, _| true);
                let _send_result = reply.send(cancellation);
            }
            ServiceCommand::Shutdown => {
                for state in lanes.values_mut() {
                    while let Some(job) = state.queued.pop_front() {
                        job.cancel.cancel();
                        (job.discard)(InvocationDiscardReason::Cancelled);
                    }
                    if let Some((_, cancel)) = &state.active {
                        cancel.cancel();
                    }
                }
                break;
            }
        }
        while active_workers < worker_count {
            let Some(lane) = ready.pop_front() else {
                break;
            };
            let Some(state) = lanes.get_mut(&lane) else {
                continue;
            };
            state.ready = false;
            if state.active.is_some() {
                continue;
            }
            let Some(job) = state.queued.pop_front() else {
                continue;
            };
            state.active = Some((job.ticket, job.cancel.clone()));
            if workers.send(job).is_err() {
                return;
            }
            active_workers += 1;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn admit_service_job(
    lanes: &mut HashMap<InvocationLane, ServiceLane>,
    ready: &mut VecDeque<InvocationLane>,
    next_ticket: &mut u64,
    max_lanes: usize,
    max_pending_per_lane: usize,
    owner: InvocationOwner,
    lane: InvocationLane,
    label: String,
    class: InvocationClass,
    cancel: Cancel,
    run: ServiceWork,
    discard: DiscardWork,
) -> Result<InvocationAdmission<()>, InvocationAdmissionError> {
    if !lanes.contains_key(&lane) && lanes.len() == max_lanes {
        return Err(InvocationAdmissionError::LaneLimit {
            lane,
            label,
            limit: max_lanes,
        });
    }
    let state = lanes.entry(lane).or_insert_with(|| ServiceLane::new(owner));
    if state.owner != owner {
        return Err(InvocationAdmissionError::LaneOwnerMismatch {
            lane,
            label,
            current: state.owner,
            submitted: owner,
        });
    }
    let ticket = InvocationTicketId(*next_ticket);
    *next_ticket = (*next_ticket)
        .checked_add(1)
        .expect("invocation service ticket space exhausted");
    let job = ServiceJob {
        ticket,
        owner,
        lane,
        label,
        class,
        cancel,
        run,
        discard,
    };
    if let Some(coalesce_key) = job.class.repeat_coalesce_key()
        && let Some(position) = state
            .queued
            .iter()
            .position(|queued| queued.class.repeat_coalesce_key() == Some(coalesce_key))
    {
        let previous = std::mem::replace(
            state
                .queued
                .get_mut(position)
                .expect("repeat position came from this lane"),
            job,
        );
        previous.cancel.cancel();
        (previous.discard)(InvocationDiscardReason::RepeatEvicted);
        return Ok(InvocationAdmission::Coalesced(InvocationTicket {
            id: ticket,
            result: PhantomData,
        }));
    }
    if state.queued.len() == max_pending_per_lane {
        if let Some(position) = state
            .queued
            .iter()
            .position(|queued| queued.class.repeat_coalesce_key().is_some())
        {
            let evicted = state
                .queued
                .remove(position)
                .expect("repeat position came from this lane");
            evicted.cancel.cancel();
            (evicted.discard)(InvocationDiscardReason::RepeatEvicted);
        } else {
            return Err(InvocationAdmissionError::LaneOverflow {
                lane,
                label: job.label,
                limit: max_pending_per_lane,
            });
        }
    }
    state.queued.push_back(job);
    schedule_service_lane(ready, lane, state);
    Ok(InvocationAdmission::Queued(InvocationTicket {
        id: ticket,
        result: PhantomData,
    }))
}

fn schedule_service_lane(
    ready: &mut VecDeque<InvocationLane>,
    lane: InvocationLane,
    state: &mut ServiceLane,
) {
    if state.active.is_none() && !state.ready && !state.queued.is_empty() {
        state.ready = true;
        ready.push_back(lane);
    }
}

fn remove_empty_service_lane(
    lanes: &mut HashMap<InvocationLane, ServiceLane>,
    lane: InvocationLane,
) {
    if lanes
        .get(&lane)
        .is_some_and(|state| state.active.is_none() && state.queued.is_empty())
    {
        lanes.remove(&lane);
    }
}

fn cancel_service_jobs(
    lanes: &mut HashMap<InvocationLane, ServiceLane>,
    ready: &mut VecDeque<InvocationLane>,
    mut matches: impl FnMut(InvocationOwner, InvocationLane) -> bool,
) -> InvocationCancellation {
    let mut cancellation = InvocationCancellation::default();
    for (lane, state) in lanes.iter_mut() {
        let mut retained = VecDeque::with_capacity(state.queued.len());
        while let Some(job) = state.queued.pop_front() {
            if matches(job.owner, job.lane) {
                job.cancel.cancel();
                (job.discard)(InvocationDiscardReason::Cancelled);
                cancellation.queued += 1;
            } else {
                retained.push_back(job);
            }
        }
        state.queued = retained;
        if let Some((_, cancel)) = &state.active
            && matches(state.owner, *lane)
        {
            cancel.cancel();
            cancellation.active += 1;
        }
    }
    ready.retain(|lane| {
        lanes
            .get(lane)
            .is_some_and(|state| state.ready && !state.queued.is_empty())
    });
    lanes.retain(|_, state| state.active.is_some() || !state.queued.is_empty());
    cancellation
}

#[cfg(any())]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::*;

    fn owner(value: u64) -> InvocationOwner {
        InvocationOwner::new(value)
    }

    fn lane(value: u64) -> InvocationLane {
        InvocationLane::new(value)
    }

    fn wait_for_cancel(cancel: &Cancel) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime builds")
            .block_on(cancel.cancelled());
    }

    #[test]
    fn service_preserves_fifo_order_inside_one_lane() {
        let service = InvocationService::with_limits("test", 2, 4, 4);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first = service
            .submit(
                owner(1),
                lane(1),
                "first",
                InvocationClass::Initial,
                move |_| {
                    entered_tx.send("first").expect("first entered");
                    release_rx.recv().expect("release first");
                    1
                },
            )
            .expect("first admitted");
        let second = service
            .submit(
                owner(1),
                lane(1),
                "second",
                InvocationClass::Initial,
                move |_| 2,
            )
            .expect("second admitted");

        assert_eq!(entered_rx.recv_timeout(Duration::from_secs(1)), Ok("first"));
        assert!(matches!(second.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release_tx.send(()).expect("release first");
        assert_eq!(
            first.recv().expect("first result"),
            InvocationCompletion::Deliver(1)
        );
        assert_eq!(
            second.recv().expect("second result"),
            InvocationCompletion::Deliver(2)
        );
    }

    #[test]
    fn service_makes_progress_in_another_lane_during_a_host_wait() {
        let service = InvocationService::with_limits("test", 2, 4, 4);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let waiting = service
            .submit(
                owner(1),
                lane(1),
                "waiting",
                InvocationClass::Initial,
                move |_| {
                    entered_tx.send(()).expect("waiting entered");
                    release_rx.recv().expect("release waiting");
                },
            )
            .expect("waiting admitted");
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiting job entered");
        let other = service
            .submit(
                owner(2),
                lane(2),
                "other",
                InvocationClass::Initial,
                move |_| 7,
            )
            .expect("other admitted");

        assert_eq!(
            other.recv().expect("other result"),
            InvocationCompletion::Deliver(7)
        );
        release_tx.send(()).expect("release waiting");
        assert_eq!(
            waiting.recv().expect("waiting result"),
            InvocationCompletion::Deliver(())
        );
    }

    #[test]
    fn service_owner_cancellation_signals_active_and_discards_queued() {
        let service = InvocationService::with_limits("test", 1, 4, 4);
        let (entered_tx, entered_rx) = mpsc::channel();
        let active = service
            .submit(
                owner(1),
                lane(1),
                "active",
                InvocationClass::Initial,
                move |cancel| {
                    entered_tx.send(()).expect("active entered");
                    wait_for_cancel(&cancel);
                    1
                },
            )
            .expect("active admitted");
        entered_rx.recv().expect("active job entered");
        let queued = service
            .submit(
                owner(1),
                lane(1),
                "queued",
                InvocationClass::Initial,
                move |_| 2,
            )
            .expect("queued admitted");

        assert_eq!(
            service.cancel_owner(owner(1)),
            InvocationCancellation {
                queued: 1,
                active: 1,
            }
        );
        assert_eq!(
            queued.recv().expect("queued discard"),
            InvocationCompletion::Discarded
        );
        assert_eq!(
            active.recv().expect("active discard"),
            InvocationCompletion::Discarded
        );
    }

    #[test]
    fn service_cancel_all_retires_every_owner_and_lane() {
        let service = InvocationService::with_limits("test", 1, 4, 4);
        let (entered_tx, entered_rx) = mpsc::channel();
        let active = service
            .submit(
                owner(1),
                lane(1),
                "active",
                InvocationClass::Initial,
                move |cancel| {
                    entered_tx.send(()).expect("active entered");
                    wait_for_cancel(&cancel);
                },
            )
            .expect("active admitted");
        entered_rx.recv().expect("active job entered");
        let queued = service
            .submit(
                owner(2),
                lane(2),
                "queued",
                InvocationClass::Initial,
                move |_| (),
            )
            .expect("queued admitted");

        assert_eq!(
            service.cancel_all(),
            InvocationCancellation {
                queued: 1,
                active: 1,
            }
        );
        assert_eq!(active.recv(), Ok(InvocationCompletion::Discarded));
        assert_eq!(queued.recv(), Ok(InvocationCompletion::Discarded));
    }

    #[test]
    fn service_replaces_a_queued_repeat_with_the_same_coalescing_key() {
        let service = InvocationService::with_limits("test", 1, 4, 4);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let active = service
            .submit(
                owner(1),
                lane(1),
                "active",
                InvocationClass::Initial,
                move |_| {
                    entered_tx.send(()).expect("active entered");
                    release_rx.recv().expect("active released");
                },
            )
            .expect("active admitted");
        entered_rx.recv().expect("active job entered");
        let replaced = service
            .submit(
                owner(1),
                lane(1),
                "first repeat",
                InvocationClass::repeat("key"),
                move |_| 1,
            )
            .expect("first repeat admitted");
        let replacement = service
            .submit(
                owner(1),
                lane(1),
                "second repeat",
                InvocationClass::repeat("key"),
                move |_| 2,
            )
            .expect("replacement repeat admitted");

        assert_eq!(replaced.recv(), Ok(InvocationCompletion::Discarded));
        release_tx.send(()).expect("release active");
        assert_eq!(active.recv(), Ok(InvocationCompletion::Deliver(())));
        assert_eq!(replacement.recv(), Ok(InvocationCompletion::Deliver(2)));
    }

    #[test]
    fn stopped_service_reports_unavailable_instead_of_overflow() {
        let (commands, receiver) = mpsc::channel();
        drop(receiver);
        let service = InvocationService {
            handle: Arc::new(InvocationServiceHandle { commands }),
        };

        let result = service.submit(
            owner(1),
            lane(2),
            "stopped",
            InvocationClass::Initial,
            |_| (),
        );
        let Err(error) = result else {
            panic!("closed scheduler must reject submission");
        };
        assert_eq!(
            error,
            InvocationAdmissionError::ServiceUnavailable {
                lane: lane(2),
                label: "stopped".to_owned(),
            }
        );
    }
}
