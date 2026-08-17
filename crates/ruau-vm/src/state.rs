//! Per-coroutine thread state, VM-global state, and call frames.
//!
//! Registers live in the heap's `StackStore`; a [`CallInfo`] is a
//! window into that stack plus the resume point. Lua calls are iterative — a
//! `CALL` pushes a `CallInfo` and the dispatch loop continues in the callee, a
//! `RETURN` pops it — so a deep Lua call chain never recurses in Rust.

use std::{collections::TryReserveError, fmt, sync::Arc};

use crate::{
    api::{HostFuture, RawGc, RawValue, RegistryRef, marker},
    func::UpVal,
    heap::{MemoryMeter, ModuleCacheKey, StackStore},
    host::HostRequests,
    object::Proto,
};

/// One activation frame: a register window into the thread stack plus the resume
/// point.
pub struct CallInfo {
    /// The closure running in this frame.
    pub closure: RawGc<marker::Closure>,
    /// The closure's proto, resolved once at frame push so the dispatch loop
    /// fetches instructions with a single arena access instead of re-deriving
    /// the proto from the closure every instruction.
    pub proto: RawGc<Proto>,
    /// Absolute index of register `R[0]` in the thread stack.
    pub base: u32,
    /// Absolute index where this frame's results are written on return — the
    /// caller's register that held the called function.
    pub result_base: u32,
    /// The logical stack top while this frame runs ordinary instructions:
    /// `base + max_stack_size`. A multret instruction lowers the live top below
    /// this; returning to the frame restores it.
    pub frame_top: u32,
    /// Next instruction index to execute (the resume PC).
    pub savedpc: usize,
    /// Expected result count, or `-1` for "as many as produced" (`multret`).
    pub nresults: i32,
    /// The extra arguments a variadic call received beyond the fixed parameters,
    /// captured at `precall` — what `...` (`GETVARARGS`) reads. Empty for a
    /// non-variadic frame or one that got only its fixed parameters.
    pub varargs: CapturedVarargs,
}

/// A protected-call continuation on the shared Lua stack.
pub struct ProtectedInfo {
    /// Absolute index where the protected call's results are written.
    pub result_base: u32,
    /// The original `CALL` result contract (`C` operand).
    pub result_count: u8,
    /// Logical top to restore if the protected call catches an error.
    pub saved_top: u32,
    /// First register owned by frames abandoned when this boundary catches.
    pub close_base: u32,
    /// Optional `xpcall` message handler. Plain `pcall` leaves this unset.
    pub handler: Option<RawValue>,
}

/// A runtime `require` continuation waiting for the required module body to
/// return. It owns the loader pin and in-flight marker until the body succeeds
/// or unwinds.
pub struct RequireInfo {
    /// Absolute index where the original `require` call writes exports.
    pub result_base: u32,
    /// The original `CALL` result contract (`C` operand).
    pub result_count: u8,
    /// Logical top to restore before placing the `require` result.
    pub saved_top: u32,
    /// One past the original `require` callee/argument register window.
    pub cleanup_end: u32,
    /// Source-provided instance key used for the export cache.
    pub instance: crate::InstanceKey,
    /// Module-source epoch used for the export cache.
    pub epoch: u64,
    /// In-flight marker cleared on success, failure, cancellation, or deadline.
    pub loading_key: ModuleCacheKey,
    /// Loader pin keeping the module closure and proto graph live while the body
    /// runs.
    pub module_pin: RegistryRef,
}

/// A call-stack entry. Protected entries count toward the same depth limit as
/// Lua frames but are not executable frames.
pub enum CallStackEntry {
    /// An ordinary executable Lua closure frame.
    Frame(CallInfo),
    /// A `pcall`/`xpcall` protected-call continuation.
    Protected(ProtectedInfo),
    /// A runtime `require` continuation that caches a module body's first return.
    Require(RequireInfo),
}

/// Captured `...` values owned by a variadic frame.
///
/// The register stack is metered separately, but these values live in side
/// storage after the callee reuses its argument registers. Charge the backing
/// capacity to the heap meter while the frame is alive and release it when the
/// frame is popped or the owning coroutine is dropped.
pub struct CapturedVarargs {
    values: Vec<RawValue>,
    meter: MemoryMeter,
    charged: usize,
}

impl CapturedVarargs {
    /// An empty vararg capture charging the supplied heap meter.
    #[must_use]
    pub fn new(meter: MemoryMeter) -> Self {
        Self {
            values: Vec::new(),
            meter,
            charged: 0,
        }
    }

    /// An empty capture with room for `capacity` values.
    ///
    /// # Errors
    /// Returns `TryReserveError` if the reservation fails before the process
    /// allocator aborts.
    pub fn with_capacity(meter: MemoryMeter, capacity: usize) -> Result<Self, TryReserveError> {
        let mut this = Self::new(meter);
        this.values.try_reserve(capacity)?;
        this.recharge();
        Ok(this)
    }

    /// Re-homes the capture to a new heap meter, preserving its current charge.
    pub(crate) fn attach_meter(&mut self, meter: MemoryMeter) {
        self.meter.adjust(self.charged, 0);
        self.meter = meter;
        self.charged = 0;
        self.recharge();
    }

    fn recharge(&mut self) {
        let now = self.values.capacity() * std::mem::size_of::<RawValue>();
        self.meter.adjust(self.charged, now);
        self.charged = now;
    }

    /// Pushes a value after [`CapturedVarargs::with_capacity`] reserved room.
    pub fn push_reserved(&mut self, value: RawValue) {
        debug_assert!(self.values.len() < self.values.capacity());
        self.values.push(value);
    }

    /// Returns a captured value by index.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&RawValue> {
        self.values.get(index)
    }

    /// Iterates over captured values.
    pub fn iter(&self) -> impl Iterator<Item = &RawValue> {
        self.values.iter()
    }

    /// The number of captured values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// The charged backing capacity in bytes.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.charged
    }
}

impl Drop for CapturedVarargs {
    fn drop(&mut self) {
        self.meter.adjust(self.charged, 0);
        self.charged = 0;
    }
}

/// Failure to reserve space for one or more call-stack entries.
#[derive(Debug)]
pub enum CallStackReserveError {
    /// The push would exceed the configured logical call-depth limit.
    Depth,
    /// The underlying vector could not reserve enough backing storage.
    Alloc,
}

/// Where a suspended `coroutine.yield` writes resume values.
pub enum ResumeSlot {
    /// Ordinary yield-call return values.
    Direct { result_base: u32, result_count: u8 },
    /// `pcall(coroutine.yield, ...)`: resume completes the pcall with `true, ...`.
    Protected { result_base: u32, result_count: u8 },
    /// Harness-only native continuation used by upstream C-yield conformance
    /// helpers. These are explicit conformance hooks, not production host APIs.
    ConformanceNative {
        result_base: u32,
        result_count: u8,
        continuation: ConformanceNativeContinuation,
    },
}

/// The suspended state for harness-only native yield helpers.
pub enum ConformanceNativeContinuation {
    /// `singleYield`: resume returns `4`.
    SingleYield,
    /// `multipleYields`: preserves the original base and current position.
    MultipleYields { base: f64, pos: i64 },
    /// `multipleYieldsWithNestedCall`: preserves the original base and which
    /// continuation step should run next.
    MultipleYieldsWithNestedCall { base: f64, state: u8 },
}

impl CallStackEntry {
    #[must_use]
    pub(crate) fn frame(&self) -> Option<&CallInfo> {
        match self {
            Self::Frame(frame) => Some(frame),
            Self::Protected(_) | Self::Require(_) => None,
        }
    }

    #[must_use]
    pub(crate) fn frame_mut(&mut self) -> Option<&mut CallInfo> {
        match self {
            Self::Frame(frame) => Some(frame),
            Self::Protected(_) | Self::Require(_) => None,
        }
    }

    #[must_use]
    pub fn protected(&self) -> Option<&ProtectedInfo> {
        match self {
            Self::Frame(_) => None,
            Self::Protected(protected) => Some(protected),
            Self::Require(_) => None,
        }
    }

    #[must_use]
    pub fn require(&self) -> Option<&RequireInfo> {
        match self {
            Self::Frame(_) | Self::Protected(_) => None,
            Self::Require(require) => Some(require),
        }
    }
}

/// Minimal debug metadata preserved after a coroutine dies from an error.
#[derive(Clone, Copy, serde::Deserialize, serde::Serialize)]
pub struct FrameSnapshot {
    /// The closure that was running in this frame.
    pub closure: RawGc<marker::Closure>,
    /// Next instruction index when the frame was abandoned.
    pub savedpc: usize,
}

/// A coroutine's lifecycle state (`lua_status`/`coroutine.status`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum CoroutineStatus {
    /// Created or yielded — resumable.
    Suspended,
    /// Currently running (the active thread).
    Running,
    /// Resumed another coroutine and is waiting for it (`"normal"`).
    Normal,
    /// Returned or errored — not resumable.
    Dead,
}

/// How a `dispatch` run ended.
#[derive(Debug)]
pub enum Step {
    /// The protected region's root frame returned these results.
    Return(Vec<RawValue>),
    /// `coroutine.yield` suspended the thread with these values; the call stack is
    /// preserved for the next resume.
    Yield(Vec<RawValue>),
    /// An async host call is pending: dispatch unwound to the async driver, which
    /// awaits the future off the VM borrow and resumes the suspended `CALL` by
    /// placing the materialized result. The call stack is preserved; the resume
    /// `savedpc` already points past the `CALL`.
    Suspend(SuspendedCall),
    /// A runtime `require` is awaiting module-source IO. Its result lands at the
    /// suspended `CALL` register once the source operation finishes.
    SuspendRequire(SuspendedRequire),
    /// A runtime `require` is waiting for another detached invocation to finish
    /// loading the same module-cache entry.
    WaitForModule(ModuleCacheKey),
    /// The cooperative scheduling quantum is spent: the async driver yields the
    /// worker (`tokio::task::yield_now`) and re-enters at the preserved `savedpc`,
    /// so a CPU-bound script cannot monopolise a runtime thread. Raised by the
    /// async root dispatch and by preemptible coroutine bodies that can propagate
    /// back to that root; never raised across synchronous native re-entry.
    Preempt,
}

/// The state the async driver holds across an await for a suspended host call:
/// the pending future plus where its result lands. The future is `'static` and
/// borrows no heap handle, so it can outlive the VM borrow.
pub struct SuspendedCall {
    /// The pending host future, awaited off the VM borrow.
    pub future: HostFuture,
    /// Scoped VM re-entry requests from an async scoped host future.
    pub(crate) host_requests: Option<HostRequests>,
    /// Registry pins minted during the synchronous half of this host call. The
    /// async driver releases any that were not consumed by result materialization.
    pub pins: Vec<RegistryRef>,
    /// The register the suspended `CALL`'s results are written to (`func_reg`).
    pub result_reg: u32,
    /// The `CALL`'s `C` operand: `0` means multret (all results), else `C-1` is
    /// the fixed result count.
    pub result_count: u8,
    /// The program counter of the suspended `CALL` itself. The resume `savedpc`
    /// on the frame already points *past* the call (so a successful resume
    /// continues correctly), so the driver rewinds to this pc to locate an async
    /// failure at the call site rather than the next instruction.
    pub call_pc: usize,
    /// One past the suspended host call's callee/argument register window.
    /// After resume, fixed-arity calls may observe fewer results than there were
    /// arguments; the dead tail must be cleared so stale heap values do not stay
    /// rooted until the next write to the temporary registers.
    pub cleanup_end: u32,
    /// Which thread owns the suspended host `CALL`.
    pub target: SuspendedTarget,
}

impl SuspendedCall {
    /// Records the bytecode call site that produced this suspension. For a direct
    /// host await this is the host call itself; for a coroutine await this is the
    /// outer `coroutine.resume` call site, while [`SuspendedCall::call_pc`] still
    /// names the inner host call in the coroutine.
    pub(crate) fn set_dispatch_call_pc(&mut self, pc: usize) {
        match &mut self.target {
            SuspendedTarget::Active => self.call_pc = pc,
            SuspendedTarget::Coroutine { resume_call_pc, .. } => *resume_call_pc = pc,
        }
    }
}

/// The pending module-source operation for a suspended runtime `require`.
pub enum SuspendedRequireStage {
    /// Canonicalizing the requested module name.
    Resolve {
        source: Arc<dyn crate::SourceProvider>,
        requester: Option<crate::ModuleId>,
        future: crate::SourceFuture<crate::ModuleId>,
    },
    /// Reading uncached module bytes after the in-flight marker has been set.
    Read {
        id: crate::ModuleId,
        instance: crate::InstanceKey,
        epoch: u64,
        loading_key: ModuleCacheKey,
        future: crate::SourceFuture<Vec<u8>>,
    },
}

/// The state the async driver holds across a suspended runtime `require`.
pub struct SuspendedRequire {
    pub stage: SuspendedRequireStage,
    /// The register the suspended `CALL`'s results are written to (`func_reg`).
    pub result_reg: u32,
    /// The `CALL`'s `C` operand: `0` means multret (all results), else `C-1` is
    /// the fixed result count.
    pub result_count: u8,
    /// The program counter of the suspended inner `require` call.
    pub call_pc: usize,
    /// One past the suspended `require` call's callee/argument register window.
    pub cleanup_end: u32,
    /// Which thread owns the suspended `require` call.
    pub target: SuspendedTarget,
}

impl SuspendedRequire {
    pub(crate) fn set_dispatch_call_pc(&mut self, pc: usize) {
        match &mut self.target {
            SuspendedTarget::Active => self.call_pc = pc,
            SuspendedTarget::Coroutine { resume_call_pc, .. } => *resume_call_pc = pc,
        }
    }
}

impl fmt::Debug for SuspendedRequire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuspendedRequire")
            .field("stage", &self.stage)
            .field("result_reg", &self.result_reg)
            .field("result_count", &self.result_count)
            .field("call_pc", &self.call_pc)
            .field("cleanup_end", &self.cleanup_end)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for SuspendedRequireStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve { .. } => f.write_str("Resolve"),
            Self::Read {
                id,
                epoch,
                loading_key,
                ..
            } => f
                .debug_struct("Read")
                .field("id", id)
                .field("epoch", epoch)
                .field("loading_key", loading_key)
                .finish_non_exhaustive(),
        }
    }
}

/// The owner of a suspended host call.
#[derive(Clone, Copy)]
pub enum SuspendedTarget {
    /// The active thread borrowed by the async driver.
    Active,
    /// A coroutine running under an outer `coroutine.resume` call. The coroutine
    /// itself owns the suspended host-call result slot; the resumer slot is where
    /// the eventual `coroutine.resume` values land.
    Coroutine {
        /// The coroutine thread to resume after the host future resolves.
        thread: RawGc<marker::Thread>,
        /// Where the outer `coroutine.resume` call writes its results.
        resume_result_reg: u32,
        /// The outer `coroutine.resume` call's result contract.
        resume_result_count: u8,
        /// The outer `coroutine.resume` call site, for locating failures in the
        /// resumer after the coroutine has produced values.
        resume_call_pc: usize,
    },
}

impl fmt::Debug for SuspendedCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuspendedCall")
            .field("result_reg", &self.result_reg)
            .field("result_count", &self.result_count)
            .field("call_pc", &self.call_pc)
            .field("cleanup_end", &self.cleanup_end)
            .field("target", &self.target)
            .field("pins", &self.pins.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for SuspendedTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("Active"),
            Self::Coroutine {
                thread,
                resume_result_reg,
                resume_result_count,
                resume_call_pc,
            } => f
                .debug_struct("Coroutine")
                .field("thread", thread)
                .field("resume_result_reg", resume_result_reg)
                .field("resume_result_count", resume_result_count)
                .field("resume_call_pc", resume_call_pc)
                .finish(),
        }
    }
}

/// A coroutine: its `CallInfo` array records the active frames; its registers
/// live in [`stacks`](Self::stacks).
pub struct Thread {
    /// This thread's register file. Each thread — the main thread and every
    /// coroutine — owns its registers, disjoint from the heap's object arenas so
    /// the interpreter can write registers while it reads constants.
    pub stacks: StackStore,
    /// The active call frames, innermost last.
    pub call_stack: Vec<CallStackEntry>,
    /// Last error stack for a dead coroutine. Luau keeps enough of an errored
    /// coroutine's call info for `debug.traceback(co)` and `debug.info(co, ...)`
    /// after `coroutine.resume` returns `false`; live registers are still cleared.
    pub error_frames: Vec<FrameSnapshot>,
    /// The logical stack top (exclusive index): the boundary a multret
    /// instruction reads to size an open argument or result list. Ordinary
    /// instructions leave it at the active frame's `frame_top`.
    pub top: u32,
    /// Upvalue cells still open over this thread's live registers, so two
    /// closures that capture the same slot by reference share one cell. A
    /// `CLOSEUPVALS` (or a returning frame) closes the ones at or above its base
    /// (`luaF_close`, `lfunc.cpp`).
    pub open_upvals: Vec<RawGc<UpVal>>,
    /// This thread's heap identity, the anchor an open upvalue records. Bound
    /// once the thread is allocated (the main thread at VM build); `None` on the
    /// placeholder that backs that allocation.
    pub id: Option<RawGc<marker::Thread>>,
    /// The global table this thread resolves `GETGLOBAL`/`GETIMPORT` against —
    /// the VM-wide environment, shared by every thread. Bound at VM build.
    pub globals: Option<RawGc<marker::Table>>,
    /// This thread's coroutine lifecycle state. The main thread starts (and stays)
    /// `Running`; a coroutine starts `Suspended`.
    pub status: CoroutineStatus,
    /// A coroutine's body function, used to build the entry frame on the first
    /// resume. `None` for the main thread.
    pub entry: Option<RawGc<marker::Closure>>,
    /// Where the next resume writes its values: the register and result count of
    /// the `coroutine.yield` call that suspended this thread (so the resume args
    /// become `yield`'s return values). `None` before the first yield.
    pub resume_slot: Option<ResumeSlot>,
    /// The depth of nested native (Rust) re-entries currently on this thread.
    /// Metamethods, host-root calls, and native builtin call paths can run a
    /// fresh `dispatch` on the Rust stack; ordinary Lua calls and bytecode
    /// `pcall` targets are iterative. Bounded so untrusted bytecode cannot
    /// overflow the host stack through chained re-entry (`nCcalls` /
    /// `LUAI_MAXCCALLS`).
    pub native_depth: u32,
    /// The `native_depth` this thread had when it was resumed — the level its
    /// coroutine body runs at. `coroutine.yield` is allowed only here (a deeper
    /// `native_depth` means a metamethod, host-root, or native builtin re-entry,
    /// across which a yield raises), so `coroutine.isyieldable` reports true
    /// exactly when `native_depth == base_native_depth` (upstream's
    /// `nCcalls <= baseCcalls`).
    pub base_native_depth: u32,
    /// The error value a coroutine died from, retained until `coroutine.close`
    /// reports it once (upstream: a `close` of an errored coroutine returns
    /// `false` and the error; a later close returns `true`). `None` for a
    /// coroutine that completed normally, is still live, or has been closed.
    pub death_error: Option<RawValue>,
    /// While this thread is running as a coroutine, the thread that resumed it (and
    /// is parked arena-resident for the duration). It makes the parked resumer
    /// **chain** a GC root: a collection during the body traces the active thread to
    /// its resumer, that resumer to *its* resumer, and so on up to the main thread,
    /// so no parked resumer is reclaimed. `Some` only between resume-start and
    /// resume-end (cleared when control returns to the resumer); `None` for the main
    /// thread and any suspended coroutine.
    pub resumer: Option<RawGc<marker::Thread>>,
    /// Async VM invocation that most recently created or resumed this coroutine.
    /// Fatal request control abandons only coroutines touched by that request,
    /// leaving retained-session coroutines from earlier successful calls intact.
    pub(crate) last_async_invocation: Option<u64>,
    /// The most recent pre-unwind traceback captured on this thread, in
    /// structured form. `ProtectedFailure` carries only the rendered text (its
    /// shape is shared by every protected surface), so the unwind stashes the
    /// paired structured capture here and the embedder error surface re-pairs
    /// the two by matching the stash's rendered text against the failure's
    /// traceback text — a stale stash can never be misattributed. Written (or
    /// cleared) on every protected unwind; plain Rust data, so it needs no GC
    /// tracing.
    pub captured_traceback: Option<crate::debug::Traceback>,
}

impl Thread {
    /// A fresh thread with no active frames and no heap identity yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stacks: StackStore::new(),
            call_stack: Vec::new(),
            error_frames: Vec::new(),
            top: 0,
            open_upvals: Vec::new(),
            id: None,
            globals: None,
            native_depth: 0,
            base_native_depth: 0,
            status: CoroutineStatus::Running,
            entry: None,
            resume_slot: None,
            death_error: None,
            resumer: None,
            last_async_invocation: None,
            captured_traceback: None,
        }
    }

    /// GC: the metered byte footprint to release when this thread is swept — its
    /// register store's charged capacity (a swept thread is an unreachable coroutine;
    /// the main thread is always rooted and never swept).
    pub(crate) fn gc_footprint(&self) -> usize {
        self.stacks.gc_footprint()
    }

    /// GC: observable live resident footprint for this thread, including active
    /// frame side storage that releases itself when frames are popped.
    pub(crate) fn gc_live_footprint(&self) -> usize {
        self.stacks.gc_footprint()
            + self
                .call_stack
                .iter()
                .filter_map(CallStackEntry::frame)
                .map(|frame| frame.varargs.charged_bytes())
                .sum::<usize>()
    }

    /// Points this thread's metered side storage at the heap's shared meter.
    pub(crate) fn attach_meter(&mut self, meter: &MemoryMeter) {
        self.stacks.attach_meter(meter.clone());
        for entry in &mut self.call_stack {
            if let Some(frame) = entry.frame_mut() {
                frame.varargs.attach_meter(meter.clone());
            }
        }
    }

    /// Reserves room for additional call-stack entries under the logical depth
    /// limit. Callers that push a protected-call burst can reserve all entries
    /// before mutating boundary state.
    pub(crate) fn reserve_call_stack_entries(
        &mut self,
        max_call_depth: u32,
        additional: usize,
    ) -> Result<(), CallStackReserveError> {
        if self.call_stack.len().saturating_add(additional) > max_call_depth as usize {
            return Err(CallStackReserveError::Depth);
        }
        self.call_stack
            .try_reserve(additional)
            .map_err(|_| CallStackReserveError::Alloc)
    }

    /// Pushes one call-stack entry after reserving room with
    /// [`Thread::reserve_call_stack_entries`].
    pub(crate) fn push_reserved_call_stack_entry(&mut self, entry: CallStackEntry) {
        debug_assert!(self.call_stack.len() < self.call_stack.capacity());
        self.call_stack.push(entry);
    }

    /// Reserves and pushes a single call-stack entry.
    pub(crate) fn push_call_stack_entry(
        &mut self,
        max_call_depth: u32,
        entry: CallStackEntry,
    ) -> Result<(), CallStackReserveError> {
        self.reserve_call_stack_entries(max_call_depth, 1)?;
        self.push_reserved_call_stack_entry(entry);
        Ok(())
    }

    /// GC: appends this thread's roots — live registers, each frame's closure and
    /// varargs, the open-upvalue cells, the globals table, and the entry closure — to
    /// the work list. The self-referential `id` handle is deliberately skipped (the
    /// collector reaches a thread through its roots, not through itself).
    pub(crate) fn gc_trace<V: crate::gc::GcVisit>(
        &self,
        v: &mut V,
    ) -> Result<(), crate::gc::GcAbort> {
        use crate::gc::GcRef;
        for value in self.stacks.gc_slots_up_to(self.gc_live_top()) {
            if let Some((child, generation)) = GcRef::from_value_gen(*value) {
                v.visit(child, generation)?;
            }
        }
        for entry in &self.call_stack {
            match entry {
                CallStackEntry::Frame(frame) => {
                    v.visit(
                        GcRef::Closure(frame.closure.index()),
                        frame.closure.generation(),
                    )?;
                    for value in frame.varargs.iter() {
                        if let Some((child, generation)) = GcRef::from_value_gen(*value) {
                            v.visit(child, generation)?;
                        }
                    }
                }
                CallStackEntry::Protected(protected) => {
                    if let Some(handler) = protected.handler
                        && let Some((child, generation)) = GcRef::from_value_gen(handler)
                    {
                        v.visit(child, generation)?;
                    }
                }
                CallStackEntry::Require(_) => {}
            }
        }
        for frame in &self.error_frames {
            v.visit(
                GcRef::Closure(frame.closure.index()),
                frame.closure.generation(),
            )?;
        }
        for upval in &self.open_upvals {
            v.visit(GcRef::UpVal(upval.index()), upval.generation())?;
        }
        if let Some(globals) = self.globals {
            v.visit(GcRef::Table(globals.index()), globals.generation())?;
        }
        if let Some(entry) = self.entry {
            v.visit(GcRef::Closure(entry.index()), entry.generation())?;
        }
        if let Some(death_error) = self.death_error
            && let Some((child, generation)) = GcRef::from_value_gen(death_error)
        {
            v.visit(child, generation)?;
        }
        // Root the parked resumer chain: a collection during this (running) thread's
        // body reaches its resumer, and the resumer's own `gc_trace` reaches the next
        // one, up to the main thread — so no parked resumer is swept under it.
        if let Some(resumer) = self.resumer {
            v.visit(GcRef::Thread(resumer.index()), resumer.generation())?;
        }
        Ok(())
    }

    fn gc_live_top(&self) -> u32 {
        let mut top = self.top;
        for entry in &self.call_stack {
            match entry {
                CallStackEntry::Frame(frame) => {
                    top = top.max(frame.frame_top);
                }
                CallStackEntry::Protected(protected) => {
                    top = top.max(protected.saved_top);
                    top = top.max(
                        protected
                            .result_base
                            .saturating_add(u32::from(protected.result_count.max(1))),
                    );
                }
                CallStackEntry::Require(require) => {
                    top = top.max(require.saved_top);
                    top = top.max(
                        require
                            .result_base
                            .saturating_add(u32::from(require.result_count.max(1))),
                    );
                }
            }
        }
        match self.resume_slot {
            Some(ResumeSlot::Direct {
                result_base,
                result_count,
            })
            | Some(ResumeSlot::Protected {
                result_base,
                result_count,
            })
            | Some(ResumeSlot::ConformanceNative {
                result_base,
                result_count,
                ..
            }) => {
                top = top.max(result_base.saturating_add(u32::from(result_count.max(1))));
            }
            None => {}
        }
        top
    }
}

impl Default for Thread {
    fn default() -> Self {
        Self::new()
    }
}
