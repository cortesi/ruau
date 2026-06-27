//! Admission and lane-pool primitives.
//!
//! A [`LanePool`] runs VM work on fixed OS threads. Each lane owns a
//! current-thread Tokio runtime and can drive `!Send` futures.

use std::{
    cmp::Ordering as CmpOrdering,
    collections::{HashMap, VecDeque},
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::JoinHandle,
};

use tokio::sync::{Notify, mpsc, oneshot};

use crate::runner::TenantId;

/// A `Send` thunk that, once on its lane, builds and drives a (possibly `!Send`)
/// future to completion. The result is delivered through a oneshot the thunk
/// captures, so the type-erased future the lane drives is `()`.
type LaneWork = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// Engine-enforced admission limits for a [`LanePool`].
///
/// `max_total` caps queued plus lane-started work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionLimits {
    /// Maximum lane-started runs pool-wide.
    pub max_in_flight: usize,
    /// Maximum lane-started runs for one tenant.
    pub max_in_flight_per_tenant: usize,
    /// Maximum queued runs pool-wide.
    pub max_queued: usize,
    /// Maximum queued runs for one tenant.
    pub max_queued_per_tenant: usize,
    /// Maximum accepted runs, queued plus lane-started, pool-wide.
    pub max_total: usize,
}

impl AdmissionLimits {
    /// No admission limits.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_in_flight: usize::MAX,
            max_in_flight_per_tenant: usize::MAX,
            max_queued: usize::MAX,
            max_queued_per_tenant: usize::MAX,
            max_total: usize::MAX,
        }
    }

    /// Conservative defaults derived from the lane count.
    #[must_use]
    pub fn fail_closed(lane_count: usize) -> Self {
        let lanes = lane_count.max(1);
        let max_in_flight = lanes.saturating_mul(4).max(4);
        let max_queued = lanes.saturating_mul(8).max(8);
        Self {
            max_in_flight,
            max_in_flight_per_tenant: lanes.saturating_mul(2).max(2),
            max_queued,
            max_queued_per_tenant: lanes.saturating_mul(2).max(2),
            max_total: max_in_flight.saturating_add(max_queued),
        }
    }

    /// In-flight caps with no queueing.
    #[must_use]
    pub const fn reject_over_in_flight(
        max_in_flight: usize,
        max_in_flight_per_tenant: usize,
    ) -> Self {
        Self {
            max_in_flight,
            max_in_flight_per_tenant,
            max_queued: 0,
            max_queued_per_tenant: 0,
            max_total: max_in_flight,
        }
    }
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// The policy result for a submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// Start immediately if caps allow it; otherwise queue when possible.
    Admit {
        /// Preferred first lane placement; out-of-range values wrap by lane count.
        lane_hint: Option<usize>,
    },
    /// Put the submission in the ready queue when possible.
    Defer {
        /// Preferred first lane placement; out-of-range values wrap by lane count.
        lane_hint: Option<usize>,
    },
    /// Reject the submission immediately.
    Reject,
}

impl AdmissionDecision {
    fn lane_hint(self) -> Option<usize> {
        match self {
            Self::Admit { lane_hint } | Self::Defer { lane_hint } => lane_hint,
            Self::Reject => None,
        }
    }
}

/// State visible to an [`AdmissionPolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionSnapshot {
    /// The tenant being submitted or considered for ready-queue dispatch.
    pub tenant: TenantId,
    /// This tenant's lane-started run count.
    pub tenant_in_flight: usize,
    /// This tenant's queued run count.
    pub tenant_queued: usize,
    /// Pool-wide lane-started run count.
    pub pool_in_flight: usize,
    /// Pool-wide queued run count.
    pub pool_queued: usize,
    /// Pool-wide accepted count, queued plus lane-started.
    pub pool_total: usize,
    /// Queue position for ready-order comparisons, or the would-be tail position
    /// for a new submission.
    pub queue_position: usize,
    /// Monotonic admission sequence for FIFO tiebreaking.
    pub sequence: u64,
    /// The number of lanes in the pool.
    pub lanes: usize,
    /// The selected first-placement lane, when this snapshot describes queued
    /// work already assigned to a lane.
    pub lane: Option<usize>,
    /// Engine-enforced caps that still apply after policy admission.
    pub limits: AdmissionLimits,
}

/// Admission policy hooks for a [`LanePool`].
///
/// Implementations should be fast. The pool still enforces caps after policy.
pub trait AdmissionPolicy: Send + Sync {
    /// Decide whether a new submission should be admitted, queued, or rejected.
    fn decide(&self, snapshot: &AdmissionSnapshot) -> AdmissionDecision {
        let limits = snapshot.limits;
        let over_total = snapshot.pool_total.saturating_add(1) > limits.max_total;
        let impossible_to_start = limits.max_in_flight == 0 || limits.max_in_flight_per_tenant == 0;
        if over_total || impossible_to_start {
            return AdmissionDecision::Reject;
        }
        if snapshot.pool_in_flight < limits.max_in_flight
            && snapshot.tenant_in_flight < limits.max_in_flight_per_tenant
        {
            return AdmissionDecision::Admit { lane_hint: None };
        }
        if snapshot.pool_queued.saturating_add(1) <= limits.max_queued
            && snapshot.tenant_queued.saturating_add(1) <= limits.max_queued_per_tenant
        {
            AdmissionDecision::Defer { lane_hint: None }
        } else {
            AdmissionDecision::Reject
        }
    }

    /// Order two ready-queue entries. Return [`CmpOrdering::Less`] when `left`
    /// should start before `right`.
    fn compare_ready(&self, left: &AdmissionSnapshot, right: &AdmissionSnapshot) -> CmpOrdering {
        left.sequence.cmp(&right.sequence)
    }
}

/// FIFO admission policy with engine backstops only.
#[derive(Debug, Default)]
pub struct DefaultAdmissionPolicy;

impl AdmissionPolicy for DefaultAdmissionPolicy {}

/// One lane: a thread running a current-thread runtime + a `LocalSet`, draining a
/// work channel and spawning each item as a lane-local task.
struct Lane {
    submit: Option<mpsc::UnboundedSender<LaneWork>>,
    thread: Option<JoinHandle<()>>,
}

impl Lane {
    fn spawn() -> Self {
        let (submit, mut work) = mpsc::unbounded_channel::<LaneWork>();
        // Build the runtime on the caller thread so a failure (fd/memory
        // pressure) surfaces at pool construction instead of silently killing
        // the lane thread and stranding its work channel.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("lane runtime construction fails only under fd/memory exhaustion");
        let thread = std::thread::spawn(move || {
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                while let Some(task) = work.recv().await {
                    // A run future is lane-local (`!Send`); `spawn_local` schedules it
                    // on this `LocalSet`, so the lane interleaves it with its siblings.
                    tokio::task::spawn_local(task());
                }
            });
        });
        Self {
            submit: Some(submit),
            thread: Some(thread),
        }
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        // Drop the sender first so the recv loop sees the channel close and
        // `block_on` returns; only then join the thread (joining first would
        // deadlock — the loop would still be awaiting). Tasks still in flight at
        // shutdown are dropped (their oneshots cancel); a graceful drain is a
        // follow-up — callers await their results before dropping the pool.
        self.submit.take();
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

#[derive(Clone, Copy, Default)]
struct TenantAdmission {
    in_flight: usize,
    queued: usize,
}

#[derive(Clone, Copy)]
struct QueuedEntry {
    ticket: u64,
    tenant: TenantId,
    lane: usize,
}

/// The pool's admission accounting: pool-wide and per-tenant in-flight/queued
/// counts, behind one lock so a decision against all caps is atomic. Also carries
/// cumulative lifetime counters for observability.
#[derive(Default)]
struct Admission {
    in_flight: usize,
    queued: usize,
    per_tenant: HashMap<TenantId, TenantAdmission>,
    queue: VecDeque<QueuedEntry>,
    next_ticket: u64,
    admitted: u64,
    deferred: u64,
    rejected: u64,
    cancelled: u64,
    finished: u64,
    lane_submit_failures: u64,
}

struct PoolCore {
    admission: Mutex<Admission>,
    notify: Notify,
    limits: AdmissionLimits,
    policy: Arc<dyn AdmissionPolicy>,
    lanes: usize,
}

impl PoolCore {
    fn new(limits: AdmissionLimits, policy: Arc<dyn AdmissionPolicy>, lanes: usize) -> Self {
        Self {
            admission: Mutex::new(Admission::default()),
            notify: Notify::new(),
            limits,
            policy,
            lanes,
        }
    }

    fn snapshot_for(
        &self,
        admission: &Admission,
        tenant: TenantId,
        queue_position: usize,
        sequence: u64,
    ) -> AdmissionSnapshot {
        let tenant_counts = admission
            .per_tenant
            .get(&tenant)
            .copied()
            .unwrap_or_default();
        AdmissionSnapshot {
            tenant,
            tenant_in_flight: tenant_counts.in_flight,
            tenant_queued: tenant_counts.queued,
            pool_in_flight: admission.in_flight,
            pool_queued: admission.queued,
            pool_total: admission.in_flight.saturating_add(admission.queued),
            queue_position,
            sequence,
            lanes: self.lanes,
            lane: None,
            limits: self.limits,
        }
    }

    fn snapshot_for_entry(
        &self,
        admission: &Admission,
        entry: QueuedEntry,
        queue_position: usize,
    ) -> AdmissionSnapshot {
        let mut snapshot = self.snapshot_for(admission, entry.tenant, queue_position, entry.ticket);
        snapshot.lane = Some(entry.lane);
        snapshot
    }

    fn can_reserve_in_flight_for(&self, admission: &Admission, tenant: TenantId) -> bool {
        let tenant_counts = admission
            .per_tenant
            .get(&tenant)
            .copied()
            .unwrap_or_default();
        admission.in_flight < self.limits.max_in_flight
            && tenant_counts.in_flight < self.limits.max_in_flight_per_tenant
    }

    fn has_total_headroom(&self, admission: &Admission) -> bool {
        admission.in_flight.saturating_add(admission.queued) < self.limits.max_total
    }

    fn can_queue_for(&self, admission: &Admission, tenant: TenantId) -> bool {
        let tenant_counts = admission
            .per_tenant
            .get(&tenant)
            .copied()
            .unwrap_or_default();
        self.limits.max_in_flight != 0
            && self.limits.max_in_flight_per_tenant != 0
            && admission.queued < self.limits.max_queued
            && tenant_counts.queued < self.limits.max_queued_per_tenant
            && admission.in_flight.saturating_add(admission.queued) < self.limits.max_total
    }

    fn reserve_in_flight(admission: &mut Admission, tenant: TenantId) {
        admission.in_flight += 1;
        admission.per_tenant.entry(tenant).or_default().in_flight += 1;
    }

    fn release_in_flight(admission: &mut Admission, tenant: TenantId) {
        admission.in_flight = admission.in_flight.saturating_sub(1);
        admission.finished = admission.finished.saturating_add(1);
        if let Some(counts) = admission.per_tenant.get_mut(&tenant) {
            counts.in_flight = counts.in_flight.saturating_sub(1);
            if counts.in_flight == 0 && counts.queued == 0 {
                admission.per_tenant.remove(&tenant);
            }
        }
    }

    fn reserve_queued(&self, admission: &mut Admission, tenant: TenantId, lane: usize) -> u64 {
        let ticket = admission.next_ticket;
        admission.next_ticket = admission.next_ticket.saturating_add(1);
        admission.queued += 1;
        admission.deferred = admission.deferred.saturating_add(1);
        admission.queue.push_back(QueuedEntry {
            ticket,
            tenant,
            lane,
        });
        admission.per_tenant.entry(tenant).or_default().queued += 1;
        ticket
    }

    fn release_queued(admission: &mut Admission, entry: QueuedEntry) {
        admission.queued = admission.queued.saturating_sub(1);
        if let Some(counts) = admission.per_tenant.get_mut(&entry.tenant) {
            counts.queued = counts.queued.saturating_sub(1);
            if counts.in_flight == 0 && counts.queued == 0 {
                admission.per_tenant.remove(&entry.tenant);
            }
        }
    }

    fn best_ready_index(&self, admission: &Admission) -> Option<usize> {
        let mut best: Option<(usize, AdmissionSnapshot)> = None;
        for (index, entry) in admission.queue.iter().copied().enumerate() {
            if !self.can_reserve_in_flight_for(admission, entry.tenant) {
                continue;
            }
            let snapshot = self.snapshot_for_entry(admission, entry, index);
            match best {
                Some((_, best_snapshot))
                    if self.compare_ready(&best_snapshot, &snapshot) != CmpOrdering::Greater => {}
                _ => best = Some((index, snapshot)),
            }
        }
        best.map(|(index, _)| index)
    }

    fn compare_ready(&self, left: &AdmissionSnapshot, right: &AdmissionSnapshot) -> CmpOrdering {
        catch_unwind(AssertUnwindSafe(|| self.policy.compare_ready(left, right)))
            .unwrap_or_else(|_| left.sequence.cmp(&right.sequence))
    }

    fn start_queued(self: &Arc<Self>, ticket: u64) -> QueuedStart {
        let mut admission = match self.admission.lock() {
            Ok(admission) => admission,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(own_index) = admission
            .queue
            .iter()
            .position(|entry| entry.ticket == ticket)
        else {
            return QueuedStart::Cancelled;
        };
        let Some(best_index) = self.best_ready_index(&admission) else {
            return QueuedStart::Wait;
        };
        if best_index != own_index {
            return QueuedStart::Wait;
        }
        let entry = admission
            .queue
            .remove(best_index)
            .expect("best index came from the queue");
        Self::release_queued(&mut admission, entry);
        Self::reserve_in_flight(&mut admission, entry.tenant);
        QueuedStart::Started(InFlightGuard {
            core: Arc::clone(self),
            tenant: entry.tenant,
        })
    }

    fn cancel_queued(&self, ticket: u64) {
        let mut admission = match self.admission.lock() {
            Ok(admission) => admission,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(index) = admission
            .queue
            .iter()
            .position(|entry| entry.ticket == ticket)
        {
            let entry = admission
                .queue
                .remove(index)
                .expect("cancel index came from the queue");
            Self::release_queued(&mut admission, entry);
            admission.cancelled = admission.cancelled.saturating_add(1);
            drop(admission);
            self.notify.notify_waiters();
        }
    }
}

enum QueuedStart {
    Started(InFlightGuard),
    Cancelled,
    Wait,
}

/// Current and cumulative pool accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneMetrics {
    /// Runs currently started on lanes pool-wide.
    pub in_flight: usize,
    /// Runs currently queued and waiting for an in-flight slot.
    pub queued: usize,
    /// Submissions accepted over the pool's lifetime.
    pub admitted: u64,
    /// Submissions accepted into the bounded queue over the pool's lifetime.
    pub deferred: u64,
    /// Submissions rejected (over a cap) over the pool's lifetime.
    pub rejected: u64,
    /// Queued submissions cancelled before they started on a lane.
    pub cancelled: u64,
    /// Runs finished (completed or dropped at shutdown) over the pool's lifetime.
    pub finished: u64,
    /// The number of lanes.
    pub lanes: usize,
    /// Lanes whose worker thread has exited. Always 0 on a healthy pool; a
    /// nonzero value means submissions routed to those lanes are failing.
    pub dead_lanes: usize,
    /// Submissions that failed because their lane's worker thread was gone.
    pub lane_submit_failures: u64,
}

struct QueuedCancellation {
    core: Option<Arc<PoolCore>>,
    ticket: u64,
}

impl QueuedCancellation {
    fn disarm(mut self) {
        self.core = None;
    }
}

impl Drop for QueuedCancellation {
    fn drop(&mut self) {
        if let Some(core) = &self.core {
            core.cancel_queued(self.ticket);
        }
    }
}

/// A submitted lane run.
///
/// Dropping this handle before a queued run starts cancels the queue ticket from
/// the caller's thread. That keeps deadline/cancellation cleanup independent of
/// whether the destination lane is currently able to poll its queued waiter.
pub(crate) struct LaneSubmission<R> {
    receiver: Option<oneshot::Receiver<R>>,
    queued: Option<QueuedCancellation>,
}

impl<R> LaneSubmission<R> {
    /// Waits for the lane result.
    pub(crate) async fn recv(mut self) -> Result<R, oneshot::error::RecvError> {
        let receiver = self.receiver.take().expect("lane submission receiver");
        receiver.await
    }

    fn into_receiver(mut self) -> oneshot::Receiver<R> {
        if let Some(queued) = self.queued.take() {
            queued.disarm();
        }
        self.receiver.take().expect("lane submission receiver")
    }
}

/// Decrements a run's in-flight counts (pool-wide and per-tenant) when it finishes,
/// or its future is dropped at shutdown — so admission accounting is exact without a
/// manual release.
struct InFlightGuard {
    core: Arc<PoolCore>,
    tenant: TenantId,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut admission = self
            .core
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        PoolCore::release_in_flight(&mut admission, self.tenant);
        self.core.notify.notify_waiters();
    }
}

/// Fixed set of lane threads for VM work.
///
/// Submissions run as lane-local tasks. Results return through a oneshot.
pub struct LanePool {
    lanes: Vec<Lane>,
    next: AtomicUsize,
    core: Arc<PoolCore>,
}

impl LanePool {
    /// Spawns `lanes` lane threads, with no admission caps.
    #[must_use]
    pub fn new(lanes: usize) -> Self {
        Self::with_admission(lanes, AdmissionLimits::unlimited())
    }

    /// Spawns `lanes` lane threads with in-flight caps and no queueing.
    #[must_use]
    pub fn with_caps(lanes: usize, max_total: usize, max_per_tenant: usize) -> Self {
        Self::with_admission(
            lanes,
            AdmissionLimits::reject_over_in_flight(max_total, max_per_tenant),
        )
    }

    /// Spawns `lanes` lane threads with explicit engine admission limits and the
    /// default FIFO policy.
    #[must_use]
    pub fn with_admission(lanes: usize, limits: AdmissionLimits) -> Self {
        Self::with_admission_policy(lanes, limits, Arc::new(DefaultAdmissionPolicy))
    }

    /// Spawns `lanes` lane threads with explicit limits and policy hooks.
    #[must_use]
    pub fn with_admission_policy(
        lanes: usize,
        limits: AdmissionLimits,
        policy: Arc<dyn AdmissionPolicy>,
    ) -> Self {
        let lane_count = lanes.max(1);
        let lanes = (0..lane_count).map(|_| Lane::spawn()).collect();
        Self {
            lanes,
            next: AtomicUsize::new(0),
            core: Arc::new(PoolCore::new(limits, policy, lane_count)),
        }
    }

    /// The number of lanes.
    #[must_use]
    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    /// A snapshot of the pool's admission accounting.
    #[must_use]
    pub fn metrics(&self) -> LaneMetrics {
        let admission = self
            .core
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        LaneMetrics {
            in_flight: admission.in_flight,
            queued: admission.queued,
            admitted: admission.admitted,
            deferred: admission.deferred,
            rejected: admission.rejected,
            cancelled: admission.cancelled,
            finished: admission.finished,
            lanes: self.lanes.len(),
            dead_lanes: self
                .lanes
                .iter()
                .filter(|lane| {
                    lane.thread
                        .as_ref()
                        .is_some_and(std::thread::JoinHandle::is_finished)
                })
                .count(),
            lane_submit_failures: admission.lane_submit_failures,
        }
    }

    /// Submits `tenant`'s work to a lane.
    ///
    /// Returns `None` when admission rejects it. `make` runs on the lane and
    /// may return a `!Send` future; the thunk and result must be `Send`.
    pub fn submit<F, Fut, R>(&self, tenant: TenantId, make: F) -> Option<oneshot::Receiver<R>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        self.submit_cancellable(tenant, make)
            .map(LaneSubmission::into_receiver)
    }

    /// Submits `tenant`'s work to a lane, returning a handle that cancels queued
    /// admission if dropped before the lane starts the work.
    pub(crate) fn submit_cancellable<F, Fut, R>(
        &self,
        tenant: TenantId,
        make: F,
    ) -> Option<LaneSubmission<R>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        let admission = self.admit(tenant)?;
        let lane = admission.lane;
        let queued_ticket = match &admission.kind {
            SubmissionKind::Immediate(_) => None,
            SubmissionKind::Queued { ticket } => Some(*ticket),
        };
        let work: LaneWork = match admission.kind {
            SubmissionKind::Immediate(guard) => {
                Box::new(move || Box::pin(run_immediate(make, guard, result_tx)))
            }
            SubmissionKind::Queued { ticket } => {
                let core = Arc::clone(&self.core);
                Box::new(move || Box::pin(run_queued(core, ticket, make, result_tx)))
            }
        };
        let submitted = self.lanes[lane]
            .submit
            .as_ref()
            .is_some_and(|submit| submit.send(work).is_ok());
        if !submitted {
            if let Some(ticket) = queued_ticket {
                self.core.cancel_queued(ticket);
            }
            self.note_lane_submit_failure();
            return None;
        }
        Some(LaneSubmission {
            receiver: Some(result_rx),
            queued: queued_ticket.map(|ticket| QueuedCancellation {
                core: Some(Arc::clone(&self.core)),
                ticket,
            }),
        })
    }

    /// Records a submission lost to a dead lane worker, so operators see the
    /// failure in [`LaneMetrics`] instead of a silently dropped result channel.
    fn note_lane_submit_failure(&self) {
        let mut admission = self
            .core
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        admission.lane_submit_failures = admission.lane_submit_failures.saturating_add(1);
    }

    fn admit(&self, tenant: TenantId) -> Option<SubmissionAdmission> {
        let mut admission = self
            .core
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = admission.next_ticket;
        let snapshot = self
            .core
            .snapshot_for(&admission, tenant, admission.queue.len(), sequence);
        let decision = catch_unwind(AssertUnwindSafe(|| self.core.policy.decide(&snapshot)))
            .unwrap_or(AdmissionDecision::Reject);
        if decision == AdmissionDecision::Reject {
            admission.rejected = admission.rejected.saturating_add(1);
            return None;
        }
        let lane = self.choose_lane(decision.lane_hint());
        let wants_queue = matches!(decision, AdmissionDecision::Defer { .. });
        if !wants_queue
            && self.core.has_total_headroom(&admission)
            && self.core.can_reserve_in_flight_for(&admission, tenant)
        {
            PoolCore::reserve_in_flight(&mut admission, tenant);
            admission.admitted = admission.admitted.saturating_add(1);
            return Some(SubmissionAdmission {
                lane,
                kind: SubmissionKind::Immediate(InFlightGuard {
                    core: Arc::clone(&self.core),
                    tenant,
                }),
            });
        }
        if !self.core.can_queue_for(&admission, tenant) {
            admission.rejected = admission.rejected.saturating_add(1);
            return None;
        }
        let ticket = self.core.reserve_queued(&mut admission, tenant, lane);
        admission.admitted = admission.admitted.saturating_add(1);
        self.core.notify.notify_waiters();
        Some(SubmissionAdmission {
            lane,
            kind: SubmissionKind::Queued { ticket },
        })
    }

    fn choose_lane(&self, lane_hint: Option<usize>) -> usize {
        lane_hint.unwrap_or_else(|| self.next.fetch_add(1, Ordering::Relaxed)) % self.lanes.len()
    }
}

struct SubmissionAdmission {
    lane: usize,
    kind: SubmissionKind,
}

enum SubmissionKind {
    Immediate(InFlightGuard),
    Queued { ticket: u64 },
}

async fn run_immediate<F, Fut, R>(make: F, guard: InFlightGuard, result_tx: oneshot::Sender<R>)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = R>,
{
    let result = make().await;
    // Release the in-flight slots *before* delivering the result, so a caller that
    // submits on receipt sees the freed capacity.
    drop(guard);
    drop(result_tx.send(result));
}

async fn run_queued<F, Fut, R>(
    core: Arc<PoolCore>,
    ticket: u64,
    make: F,
    mut result_tx: oneshot::Sender<R>,
) where
    F: FnOnce() -> Fut,
    Fut: Future<Output = R>,
{
    let notified = core.notify.notified();
    tokio::pin!(notified);
    let guard = loop {
        notified.as_mut().enable();
        match core.start_queued(ticket) {
            QueuedStart::Started(guard) => break guard,
            QueuedStart::Cancelled => return,
            QueuedStart::Wait => {}
        }
        tokio::select! {
            () = &mut notified => {
                notified.set(core.notify.notified());
            }
            () = result_tx.closed() => {
                core.cancel_queued(ticket);
                return;
            }
        }
    };
    if result_tx.is_closed() {
        drop(guard);
        return;
    }
    let result = make().await;
    drop(guard);
    drop(result_tx.send(result));
}

#[cfg(any())]
mod tests {
    use super::*;

    struct PanicDecidePolicy;

    impl AdmissionPolicy for PanicDecidePolicy {
        fn decide(&self, _snapshot: &AdmissionSnapshot) -> AdmissionDecision {
            panic!("policy decide failed");
        }
    }

    struct PanicComparePolicy;

    impl AdmissionPolicy for PanicComparePolicy {
        fn decide(&self, snapshot: &AdmissionSnapshot) -> AdmissionDecision {
            DefaultAdmissionPolicy.decide(snapshot)
        }

        fn compare_ready(
            &self,
            _left: &AdmissionSnapshot,
            _right: &AdmissionSnapshot,
        ) -> CmpOrdering {
            panic!("policy compare failed");
        }
    }

    #[test]
    fn lane_pool_runs_generic_work_across_lanes() {
        let pool = LanePool::new(2);
        let pending = (0..8_u32)
            .map(|value| {
                pool.submit(TenantId(u64::from(value)), move || async move { value + 1 })
                    .expect("work admitted")
            })
            .collect::<Vec<_>>();

        for (index, receiver) in pending.into_iter().enumerate() {
            assert_eq!(
                receiver.blocking_recv().expect("lane result"),
                index as u32 + 1
            );
        }
    }

    #[test]
    fn in_flight_caps_reject_over_limit() {
        let pool = LanePool::with_caps(1, 1, usize::MAX);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let held = pool
            .submit(TenantId(0), move || async move {
                release_rx.recv().expect("release signal");
                1_u32
            })
            .expect("first run admitted");

        assert!(
            pool.submit(TenantId(1), || async { 2_u32 }).is_none(),
            "second run should be rejected while the only in-flight slot is held"
        );

        release_tx.send(()).expect("release held run");
        assert_eq!(held.blocking_recv().expect("held result"), 1);
    }

    #[test]
    fn queued_work_starts_after_in_flight_slot_is_released() {
        let pool = LanePool::with_admission(
            1,
            AdmissionLimits {
                max_in_flight: 1,
                max_in_flight_per_tenant: 1,
                max_queued: 1,
                max_queued_per_tenant: 1,
                max_total: 2,
            },
        );
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let held = pool
            .submit(TenantId(0), move || async move {
                release_rx.recv().expect("release signal");
                1_u32
            })
            .expect("first run admitted");
        let queued = pool
            .submit(TenantId(1), || async { 2_u32 })
            .expect("second run queued");

        assert_eq!(pool.metrics().queued, 1);
        release_tx.send(()).expect("release held run");
        assert_eq!(held.blocking_recv().expect("held result"), 1);
        assert_eq!(queued.blocking_recv().expect("queued result"), 2);
    }

    #[test]
    fn dropping_cancellable_queued_work_removes_ticket() {
        let pool = LanePool::with_admission(
            1,
            AdmissionLimits {
                max_in_flight: 1,
                max_in_flight_per_tenant: 1,
                max_queued: 1,
                max_queued_per_tenant: 1,
                max_total: 2,
            },
        );
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let held = pool
            .submit(TenantId(0), move || async move {
                release_rx.recv().expect("release signal");
                1_u32
            })
            .expect("first run admitted");
        let queued = pool
            .submit_cancellable(TenantId(1), || async { 2_u32 })
            .expect("second run queued");

        assert_eq!(pool.metrics().queued, 1);
        drop(queued);
        let metrics = pool.metrics();
        assert_eq!(metrics.queued, 0);
        assert_eq!(metrics.cancelled, 1);

        release_tx.send(()).expect("release held run");
        assert_eq!(held.blocking_recv().expect("held result"), 1);
    }

    #[test]
    fn policy_decide_panic_rejects_without_poisoning_pool() {
        let pool = LanePool::with_admission_policy(
            1,
            AdmissionLimits::unlimited(),
            Arc::new(PanicDecidePolicy),
        );

        assert!(
            pool.submit(TenantId(1), || async { 1_u32 }).is_none(),
            "panicking policy is treated as an explicit rejection"
        );
        let metrics = pool.metrics();
        assert_eq!(metrics.rejected, 1);
        assert_eq!(metrics.in_flight, 0);
        assert_eq!(metrics.queued, 0);
    }

    #[test]
    fn policy_compare_panic_falls_back_to_fifo_without_poisoning_pool() {
        let pool = LanePool::with_admission_policy(
            1,
            AdmissionLimits {
                max_in_flight: 1,
                max_in_flight_per_tenant: usize::MAX,
                max_queued: 2,
                max_queued_per_tenant: usize::MAX,
                max_total: 3,
            },
            Arc::new(PanicComparePolicy),
        );
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let held = pool
            .submit(TenantId(0), move || async move {
                release_rx.recv().expect("release signal");
                0_u32
            })
            .expect("first run admitted");
        let first_queued = pool
            .submit(TenantId(1), || async { 1_u32 })
            .expect("first queued run admitted");
        let second_queued = pool
            .submit(TenantId(2), || async { 2_u32 })
            .expect("second queued run admitted");

        assert_eq!(pool.metrics().queued, 2);
        release_tx.send(()).expect("release held run");
        assert_eq!(held.blocking_recv().expect("held result"), 0);
        assert_eq!(
            first_queued.blocking_recv().expect("first queued result"),
            1
        );
        assert_eq!(
            second_queued.blocking_recv().expect("second queued result"),
            2
        );
        let metrics = pool.metrics();
        assert_eq!(metrics.rejected, 0);
        assert_eq!(metrics.queued, 0);
    }

    #[tokio::test]
    async fn dead_lane_surfaces_in_lane_metrics() {
        let mut pool = LanePool::new(1);
        let healthy = pool.metrics();
        assert_eq!(healthy.dead_lanes, 0);
        assert_eq!(healthy.lane_submit_failures, 0);

        // Kill the lane worker: replace its sender with one whose receiver is
        // gone, and drop the original sender so the worker loop exits.
        let (dead_tx, dead_rx) = mpsc::unbounded_channel::<LaneWork>();
        drop(dead_rx);
        let original = pool.lanes[0].submit.replace(dead_tx);
        drop(original);
        let thread_done = || {
            pool.lanes[0]
                .thread
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
        };
        for _ in 0..200 {
            if thread_done() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let receiver = pool.submit(TenantId(1), || async { 1_u32 });
        assert!(receiver.is_none(), "a dead lane cannot accept work");
        let metrics = pool.metrics();
        assert_eq!(metrics.lane_submit_failures, 1);
        assert_eq!(metrics.dead_lanes, 1, "the exited worker thread is visible");

        // Forget the dead sender so Drop's join doesn't hang on a lane whose
        // worker already exited.
        pool.lanes[0].submit.take();
    }
}
