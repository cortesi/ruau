//! The async driver: the asynchronous analog of the synchronous
//! [`run`](crate::call::run).
//!
//! It runs [`dispatch`] to a [`Step`]; on [`Step::Suspend`] it awaits the pending
//! host future *off the VM borrow* — the future is `'static` and holds only
//! owned data or registry pins, not borrowed heap handles — then materializes
//! the result into the suspended `CALL`'s result register and re-enters
//! `dispatch`, which resumes at the saved program counter past the `CALL`.
//!
//! The driver enforces wall-clock deadline and cancellation while a host future
//! is parked. Host calls that suspend inside a Lua coroutine park that coroutine
//! and resume the outer `coroutine.resume` call after the await.
//!
//! # Nested host re-entry
//!
//! A pending async host call may re-enter the VM through its
//! [`AsyncHostContext`](crate::AsyncHostContext): scoped segments via `AsyncHostContext::scope` and
//! protected callback invocations via `AsyncHostContext::call_protected`. The driver
//! services these requests one at a time while the host future is parked, and
//! each `call_protected` runs on a fresh rooted callback thread under this same
//! drive loop, so re-entry composes:
//!
//! - **Sync host function in a re-entry** — supported; it is ordinary dispatch
//!   inside the nested protected run.
//! - **Async host function in a re-entry** — supported; the nested run suspends
//!   and awaits it (with its own `AsyncHostContext`) while the outer host call stays
//!   pending.
//! - **Re-entry within a re-entry** — supported; each level allocates a fresh
//!   callback thread and charges one unit of `max_native_depth` (seeded from
//!   the suspended thread, so recursion accumulates across levels). A predicate
//!   that re-enters recursively fails closed with a catchable
//!   `"stack overflow (async host re-entry)"` runtime error and unwinds
//!   cleanly — the VM is not poisoned and stays reusable — rather than
//!   exhausting the Rust stack through the nested poll chain.
//! - **Governance during re-entry** — the wall-clock deadline and cancellation
//!   token gate every nested await; cancellation is also polled at the
//!   synchronous dispatch safepoint, and a busy pure-Lua segment (such as a
//!   re-entrant predicate loop) yields the worker on `Limits::quantum`, where
//!   the wall-clock deadline is enforced as well. Deadline and cancellation
//!   remain fatal (uncatchable) and unwind through every nesting level.
//!
//! Unsupported: a `AsyncHostContext`'s requests are serviced only while *that* host
//! call's await is the innermost active one. A request sent from a deeper
//! nesting level on an outer call's `AsyncHostContext` (for example, a re-entrant
//! callback's host function using a captured outer context) queues until the
//! inner nesting unwinds; if the inner nesting itself awaits that reply, only
//! the deadline or cancellation ends the wait — the same failure mode as a host
//! future that never resolves, and the reason async entry points should run
//! under a deadline or cancel token. Within one host call, concurrent requests
//! (for example from spawned tasks sharing a cloned `AsyncHostContext`) are serviced
//! serially in send order, not in parallel. Async host calls reached through
//! the synchronous entry points or across a native (C-call) boundary fail
//! closed with a catchable runtime error; see [`run`](crate::call::run) and
//! `call_value`.

use std::{
    panic::AssertUnwindSafe,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use crate::{
    api::{HostError, HostReturn, OwnedValue, RawGc, RawValue, RegistryRef, Unwind, marker},
    call::{
        Exec, ProtectedFailure, RaisedError, RuntimeErrorKind, catch_protected_error, err,
        err_deadline, err_stopped, error_payload_from_message, materialize, materialize_owned,
        place_results, prepare_result_copy, push_call_entry, release_owned_pins, root_frame,
        root_function_frame, unwind_protected_failure,
    },
    cancel::{Cancel, StopReason},
    coroutine::{self, CoroutineStep},
    execute::{DispatchMode, dispatch},
    heap::Heap,
    host::{
        HostProtectedCallRequest, HostProtectedCallResult, HostRequest, HostRequests,
        HostScopeRequest, HostScriptError, ProtectedArgsOperation,
    },
    scope::{HostEntry, RuntimeError, Scope},
    state::{
        CallInfo, CallStackEntry, CoroutineStatus, Step, SuspendedCall, SuspendedRequire,
        SuspendedRequireStage, SuspendedTarget, Thread,
    },
};

/// The wall-clock deadline and cancellation handle the driver applies while a host
/// future is pending. A `None` deadline parks indefinitely.
/// `Deadline::Logical` (deterministic mode) is reserved for the model harness and
/// not yet enforced anywhere — only a wall-clock deadline gates a real await; the
/// cancellation token is additionally polled at the synchronous dispatch safepoint.
#[derive(Default)]
pub struct Governance {
    /// The absolute instant the request must finish by, if wall-clock bounded.
    pub deadline: Option<Instant>,
    /// The request's cancellation token, tripped to abort a parked await.
    pub cancel: Option<Cancel>,
}

/// Runs `main` to completion on the async driver, awaiting any pending async host
/// calls. The asynchronous analog of [`run`](crate::call::run).
///
/// # Errors
/// Returns the [`Unwind`] of an uncaught runtime error or a failed async host
/// call, its message interned as a Lua string.
#[cfg(any(test, feature = "conformance"))]
pub async fn run_async(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    main: RawGc<marker::Closure>,
    governance: &Governance,
    host_entry: HostEntry<'_>,
) -> Result<Vec<RawValue>, Unwind> {
    let outcome = protected_async_main(heap, main_thread, main, governance, host_entry, None).await;
    unwind_driver_outcome(heap, outcome)
}

/// Runs `main` on the async driver in a protected mode: catchable script errors
/// become the inner `Err`, while fatal control-flow errors stay outer `Err`s.
pub async fn run_async_protected(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    main: RawGc<marker::Closure>,
    governance: &Governance,
    host_entry: HostEntry<'_>,
    max_traceback_bytes: usize,
) -> Result<Result<Vec<RawValue>, ProtectedFailure>, Unwind> {
    let outcome = protected_async_main(
        heap,
        main_thread,
        main,
        governance,
        host_entry,
        Some(max_traceback_bytes),
    )
    .await;
    protect_driver_outcome(heap, outcome)
}

/// Runs a raw Lua closure with arguments on the async driver in protected mode.
/// Catchable script/host failures become the inner `Err`; fatal control-flow
/// errors stay outer `Err`s.
pub async fn run_async_function_protected(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    func: RawValue,
    args: Vec<RawValue>,
    governance: &Governance,
    host_entry: HostEntry<'_>,
    max_traceback_bytes: usize,
) -> Result<Result<Vec<RawValue>, ProtectedFailure>, Unwind> {
    let outcome = protected_async_function(
        heap,
        main_thread,
        func,
        args,
        governance,
        host_entry,
        Some(max_traceback_bytes),
    )
    .await;
    protect_driver_outcome(heap, outcome)
}

enum DriverError {
    Runtime(ProtectedFailure),
    Poison,
}

/// Maps a driver outcome to the unprotected entry-point shape: every runtime
/// failure unwinds.
fn unwind_driver_outcome(
    heap: &mut Heap,
    outcome: Result<Vec<RawValue>, DriverError>,
) -> Result<Vec<RawValue>, Unwind> {
    match outcome {
        Ok(results) => Ok(results),
        Err(DriverError::Runtime(failure)) => {
            let kind = failure.error.kind;
            Err(Unwind {
                error: materialize(heap, failure.error),
                kind,
            })
        }
        Err(DriverError::Poison) => Err(panic_poison_unwind()),
    }
}

/// Maps a driver outcome to the protected entry-point shape: catchable script
/// errors become the inner `Err`, fatal control flow stays the outer `Err`.
fn protect_driver_outcome(
    heap: &mut Heap,
    outcome: Result<Vec<RawValue>, DriverError>,
) -> Result<Result<Vec<RawValue>, ProtectedFailure>, Unwind> {
    match outcome {
        Err(DriverError::Runtime(failure)) if failure.error.is_catchable() => Ok(Err(failure)),
        outcome => unwind_driver_outcome(heap, outcome).map(Ok),
    }
}

impl From<RaisedError> for DriverError {
    fn from(error: RaisedError) -> Self {
        Self::Runtime(ProtectedFailure {
            error,
            traceback: None,
        })
    }
}

impl From<ProtectedFailure> for DriverError {
    fn from(failure: ProtectedFailure) -> Self {
        Self::Runtime(failure)
    }
}

type DriverExec<T> = Result<T, DriverError>;

fn panic_poison_unwind() -> Unwind {
    Unwind {
        error: RawValue::Nil,
        kind: RuntimeErrorKind::PanicPoison,
    }
}

fn with_thread_segment<T>(
    heap: &mut Heap,
    handle: RawGc<marker::Thread>,
    host_entry: HostEntry<'_>,
    f: impl FnOnce(&mut Heap, &mut Thread) -> DriverExec<T>,
) -> DriverExec<T> {
    let _ = host_entry;
    heap.drain_releases();
    let Some(mut thread) = heap.take_thread(handle) else {
        return Err(DriverError::Poison);
    };
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| f(heap, &mut thread)));
    let restored = heap.put_thread(handle, thread);
    if !restored {
        return Err(DriverError::Poison);
    }
    match outcome {
        Ok(result) => result,
        Err(_) => Err(DriverError::Poison),
    }
}

#[derive(Clone, Copy)]
struct ProtectedState {
    floor: usize,
    saved_top: u32,
}

#[derive(Clone, Copy)]
struct DriverRoot<'a> {
    main_thread: RawGc<marker::Thread>,
    state: ProtectedState,
    host_entry: HostEntry<'a>,
    traceback_limit: Option<usize>,
    allow_module_wait: bool,
}

#[derive(Clone, Copy)]
struct AwaitSite {
    result_reg: u32,
    result_count: u8,
    call_pc: usize,
    cleanup_end: u32,
}

#[derive(Clone, Copy)]
struct ResumeSite {
    result_reg: u32,
    result_count: u8,
    call_pc: usize,
}

struct RequireReadReady {
    id: crate::ModuleId,
    instance: crate::InstanceKey,
    epoch: u64,
    loading_key: crate::heap::ModuleCacheKey,
    source: Vec<u8>,
}

enum RootStep {
    Return(Vec<RawValue>),
    Preempt,
    Suspend(SuspendedCall),
    SuspendRequire(SuspendedRequire),
    WaitForModule(crate::heap::ModuleCacheKey),
}

/// Runs one root-async dispatch segment and applies the protected unwind policy
/// shared by attached and detached drivers.
fn dispatch_root_segment(heap: &mut Heap, root: DriverRoot<'_>) -> DriverExec<RootStep> {
    with_thread_segment(
        heap,
        root.main_thread,
        root.host_entry,
        |heap, thread| match dispatch(
            heap,
            thread,
            root.state.floor,
            DispatchMode::RootAsync,
            root.host_entry,
        ) {
            Ok(Step::Return(results)) => Ok(RootStep::Return(results)),
            Ok(Step::Yield(_)) => {
                let error = err("attempt to yield across the main thread");
                Err(unwind_protected_failure(
                    heap,
                    thread,
                    root.state.floor,
                    root.state.saved_top,
                    error,
                    root.traceback_limit,
                )
                .into())
            }
            Ok(Step::WaitForModule(loading_key)) if root.allow_module_wait => {
                Ok(RootStep::WaitForModule(loading_key))
            }
            Ok(Step::WaitForModule(_)) => {
                let error = err("required module is already loading");
                Err(unwind_protected_failure(
                    heap,
                    thread,
                    root.state.floor,
                    root.state.saved_top,
                    error,
                    root.traceback_limit,
                )
                .into())
            }
            Ok(Step::Preempt) => Ok(RootStep::Preempt),
            Ok(Step::Suspend(call)) => Ok(RootStep::Suspend(call)),
            Ok(Step::SuspendRequire(require)) => Ok(RootStep::SuspendRequire(require)),
            Err(error) => Err(unwind_protected_failure(
                heap,
                thread,
                root.state.floor,
                root.state.saved_top,
                error,
                root.traceback_limit,
            )
            .into()),
        },
    )
}

fn start_protected_async(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    host_entry: HostEntry<'_>,
    build_frame: impl FnOnce(&mut Heap, &mut Thread) -> Exec<CallInfo>,
) -> DriverExec<ProtectedState> {
    with_thread_segment(heap, main_thread, host_entry, |heap, thread| {
        let frame = build_frame(heap, thread)?;
        let floor = thread.call_stack.len();
        let saved_top = thread.top;
        let frame_top = frame.frame_top;
        push_call_entry(heap, thread, CallStackEntry::Frame(frame))?;
        thread.top = frame_top;
        Ok(ProtectedState { floor, saved_top })
    })
}

async fn protected_async_main(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    main: RawGc<marker::Closure>,
    governance: &Governance,
    host_entry: HostEntry<'_>,
    traceback_limit: Option<usize>,
) -> DriverExec<Vec<RawValue>> {
    let state = start_protected_async(heap, main_thread, host_entry, |heap, thread| {
        root_frame(heap, thread, main)
    })?;
    drive_protected_async(
        heap,
        main_thread,
        governance,
        state,
        host_entry,
        traceback_limit,
    )
    .await
}

async fn protected_async_function(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    func: RawValue,
    args: Vec<RawValue>,
    governance: &Governance,
    host_entry: HostEntry<'_>,
    traceback_limit: Option<usize>,
) -> DriverExec<Vec<RawValue>> {
    let state = start_protected_async(heap, main_thread, host_entry, |heap, thread| {
        root_function_frame(heap, thread, func, &args)
    })?;
    drop(args);
    drive_protected_async(
        heap,
        main_thread,
        governance,
        state,
        host_entry,
        traceback_limit,
    )
    .await
}

/// The async protected region: it drives `dispatch`/await/resume in a loop and,
/// on a failure, unwinds the abandoned frames so the thread stays reusable — the
/// async counterpart of [`protected`](crate::call) reusing the same unwind core.
async fn drive_protected_async(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    governance: &Governance,
    state: ProtectedState,
    host_entry: HostEntry<'_>,
    traceback_limit: Option<usize>,
) -> DriverExec<Vec<RawValue>> {
    loop {
        // Synchronous segment: run to the next step while borrowing the VM. This is
        // the driver's root-async dispatch, so it may return `Preempt`.
        let root_step = dispatch_root_segment(
            heap,
            DriverRoot {
                main_thread,
                state,
                host_entry,
                traceback_limit,
                allow_module_wait: false,
            },
        )?;
        let mut suspended = match root_step {
            RootStep::Return(results) => return Ok(results),
            RootStep::Preempt => {
                // The preemption checkpoint also enforces the wall-clock deadline,
                // so a busy pure-Lua segment that never awaits — such as a
                // re-entrant predicate loop — cannot outrun it. Cancellation needs
                // no check here: dispatch polls it at the batched safepoint.
                if let Some(deadline) = governance.deadline
                    && Instant::now() >= deadline
                {
                    return Err(unwind_main(
                        heap,
                        main_thread,
                        state,
                        host_entry,
                        traceback_limit,
                        err_deadline("deadline exceeded at a preemption checkpoint"),
                    ));
                }
                tokio::task::yield_now().await;
                continue;
            }
            RootStep::Suspend(call) => DriverSuspend::Call(call),
            RootStep::SuspendRequire(require) => DriverSuspend::Require(require),
            RootStep::WaitForModule(_) => {
                unreachable!("ordinary async execution rejects shared module loads")
            }
        };

        loop {
            let resumed = match suspended {
                DriverSuspend::Call(call) => {
                    resume_awaited_call(
                        heap,
                        main_thread,
                        call,
                        governance,
                        state,
                        host_entry,
                        traceback_limit,
                    )
                    .await
                }
                DriverSuspend::Require(require) => {
                    let root = DriverRoot {
                        main_thread,
                        state,
                        host_entry,
                        traceback_limit,
                        allow_module_wait: false,
                    };
                    resume_awaited_require(heap, require, governance, root).await
                }
            };
            match resumed {
                Ok(DriverResume::Continue) => break,
                Ok(DriverResume::Suspend(next)) => suspended = *next,
                Ok(DriverResume::WaitForModule(_)) => {
                    unreachable!("ordinary async execution cannot share a module load")
                }
                Ok(DriverResume::WaitForCoroutine(_)) => {
                    unreachable!("ordinary async execution cannot share a module load")
                }
                Err(error) => return Err(error),
            }
        }
    }
}

enum DriverSuspend {
    Call(SuspendedCall),
    Require(SuspendedRequire),
}

enum DriverResume {
    Continue,
    Suspend(Box<DriverSuspend>),
    WaitForModule(ModuleWait),
    WaitForCoroutine(CoroutineWait),
}

struct ModuleWait {
    loading_key: crate::heap::ModuleCacheKey,
    source: std::sync::Arc<dyn crate::SourceProvider>,
    id: crate::ModuleId,
    requester: Option<crate::ModuleId>,
    site: AwaitSite,
    target: SuspendedTarget,
}

struct CoroutineWait {
    loading_key: crate::heap::ModuleCacheKey,
    coroutine_thread: RawGc<marker::Thread>,
    resume_site: ResumeSite,
}

impl DriverResume {
    fn suspend(next: DriverSuspend) -> Self {
        Self::Suspend(Box::new(next))
    }
}

fn detached_phase_from_resume(resumed: DriverExec<DriverResume>) -> DriverExec<DetachedPhase> {
    resumed.map(|resume| match resume {
        DriverResume::Continue => DetachedPhase::Dispatch,
        DriverResume::Suspend(next) => detached_phase(*next),
        DriverResume::WaitForModule(wait) => DetachedPhase::WaitForModule(wait),
        DriverResume::WaitForCoroutine(wait) => DetachedPhase::WaitForCoroutine(wait),
    })
}

async fn resume_awaited_call(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    call: SuspendedCall,
    governance: &Governance,
    state: ProtectedState,
    host_entry: HostEntry<'_>,
    traceback_limit: Option<usize>,
) -> DriverExec<DriverResume> {
    let SuspendedCall {
        future,
        host_requests,
        pins,
        result_reg,
        result_count,
        call_pc,
        cleanup_end,
        target,
    } = call;
    let site = AwaitSite {
        result_reg,
        result_count,
        call_pc,
        cleanup_end,
    };
    let scope_thread = match &target {
        SuspendedTarget::Active => main_thread,
        SuspendedTarget::Coroutine { thread, .. } => *thread,
    };
    let outcome = await_governed(
        future,
        host_requests,
        governance,
        heap,
        main_thread,
        scope_thread,
        host_entry,
    )
    .await?;
    resume_awaited_call_result(
        heap,
        main_thread,
        pins,
        site,
        target,
        outcome,
        state,
        host_entry,
        traceback_limit,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn resume_awaited_call_result(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    mut pins: Vec<RegistryRef>,
    site: AwaitSite,
    target: SuspendedTarget,
    outcome: Result<HostReturn, AwaitFailure>,
    state: ProtectedState,
    host_entry: HostEntry<'_>,
    traceback_limit: Option<usize>,
    allow_module_wait: bool,
) -> DriverExec<DriverResume> {
    match target {
        SuspendedTarget::Active => {
            with_thread_segment(heap, main_thread, host_entry, |heap, thread| {
                resume_active_await(
                    heap,
                    thread,
                    pins,
                    site,
                    outcome,
                    state,
                    traceback_limit,
                    host_entry,
                )
            })
        }
        SuspendedTarget::Coroutine {
            thread: coroutine_thread,
            resume_result_reg,
            resume_result_count,
            resume_call_pc,
        } => {
            heap.drain_releases();
            let Some(mut co) = heap.take_thread(coroutine_thread) else {
                release_pins(heap, &mut pins);
                return Err(unwind_main(
                    heap,
                    main_thread,
                    state,
                    host_entry,
                    traceback_limit,
                    err("suspended coroutine is not resident"),
                ));
            };
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                Ok(resume_coroutine_await(
                    heap, &mut co, pins, site, outcome, host_entry,
                )?)
            }));
            let restored = heap.put_thread(coroutine_thread, co);
            if !restored {
                return Err(DriverError::Poison);
            }
            let step = match outcome {
                Ok(Ok(step)) => step,
                Ok(Err(DriverError::Runtime(error))) => {
                    return Err(unwind_main(
                        heap,
                        main_thread,
                        state,
                        host_entry,
                        traceback_limit,
                        error.error,
                    ));
                }
                Ok(Err(DriverError::Poison)) | Err(_) => return Err(DriverError::Poison),
            };
            match step {
                CoroutineStep::Values(values) => place_coroutine_resume_results(
                    heap,
                    main_thread,
                    state,
                    host_entry,
                    traceback_limit,
                    ResumeSite {
                        result_reg: resume_result_reg,
                        result_count: resume_result_count,
                        call_pc: resume_call_pc,
                    },
                    &values,
                ),
                CoroutineStep::Suspend(mut next) => {
                    next.target = SuspendedTarget::Coroutine {
                        thread: coroutine_thread,
                        resume_result_reg,
                        resume_result_count,
                        resume_call_pc,
                    };
                    Ok(DriverResume::suspend(DriverSuspend::Call(next)))
                }
                CoroutineStep::SuspendRequire(mut next) => {
                    next.target = SuspendedTarget::Coroutine {
                        thread: coroutine_thread,
                        resume_result_reg,
                        resume_result_count,
                        resume_call_pc,
                    };
                    Ok(DriverResume::suspend(DriverSuspend::Require(next)))
                }
                CoroutineStep::WaitForModule(loading_key) if allow_module_wait => {
                    Ok(DriverResume::WaitForCoroutine(CoroutineWait {
                        loading_key,
                        coroutine_thread,
                        resume_site: ResumeSite {
                            result_reg: resume_result_reg,
                            result_count: resume_result_count,
                            call_pc: resume_call_pc,
                        },
                    }))
                }
                CoroutineStep::WaitForModule(_) => Err(unwind_main(
                    heap,
                    main_thread,
                    state,
                    host_entry,
                    traceback_limit,
                    err("required module is already loading"),
                )),
                CoroutineStep::Preempt => {
                    unreachable!(
                        "post-await coroutine resumes are non-preemptible until the resumer chain is rooted"
                    )
                }
            }
        }
    }
}

enum SourceAwait<T> {
    Ready(crate::SourceResult<T>),
    Stopped(StopReason),
}

async fn await_module_source<T>(
    mut future: crate::SourceFuture<T>,
    governance: &Governance,
) -> SourceAwait<T> {
    tokio::select! {
        biased;
        result = &mut future => SourceAwait::Ready(result),
        () = deadline_elapsed(governance.deadline) => SourceAwait::Stopped(stop_for_deadline(governance)),
        () = cancellation(governance.cancel.as_ref()) => SourceAwait::Stopped(stop_for_cancel(governance.cancel.as_ref())),
    }
}

async fn resume_awaited_require(
    heap: &mut Heap,
    require: SuspendedRequire,
    governance: &Governance,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    let SuspendedRequire {
        stage,
        result_reg,
        result_count,
        call_pc,
        cleanup_end,
        target,
    } = require;
    let site = AwaitSite {
        result_reg,
        result_count,
        call_pc,
        cleanup_end,
    };
    match stage {
        SuspendedRequireStage::Resolve {
            source,
            requester,
            future,
        } => match await_module_source(future, governance).await {
            SourceAwait::Ready(Ok(id)) => {
                resume_resolved_require(heap, &target, &source, id, &requester, site, root)
            }
            SourceAwait::Ready(Err(error)) => resume_require_error(
                heap,
                &target,
                site,
                root,
                crate::builtins::require_resolve_error(&error),
            ),
            SourceAwait::Stopped(reason) => {
                resume_require_error(heap, &target, site, root, err_stopped(reason))
            }
        },
        SuspendedRequireStage::Read {
            id,
            instance,
            epoch,
            loading_key,
            future,
        } => match await_module_source(future, governance).await {
            SourceAwait::Ready(Ok(source)) => resume_read_require(
                heap,
                &target,
                RequireReadReady {
                    id,
                    instance,
                    epoch,
                    loading_key,
                    source,
                },
                site,
                root,
            ),
            SourceAwait::Ready(Err(error)) => {
                let error =
                    crate::builtins::finish_require_read_error(heap, &id, &loading_key, &error);
                resume_require_error(heap, &target, site, root, error)
            }
            SourceAwait::Stopped(reason) => {
                crate::builtins::clear_require_loading(heap, &loading_key);
                resume_require_error(heap, &target, site, root, err_stopped(reason))
            }
        },
    }
}

fn resume_resolved_require(
    heap: &mut Heap,
    target: &SuspendedTarget,
    source: &std::sync::Arc<dyn crate::SourceProvider>,
    id: crate::ModuleId,
    requester: &Option<crate::ModuleId>,
    site: AwaitSite,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    match target {
        SuspendedTarget::Active => {
            with_thread_segment(heap, root.main_thread, root.host_entry, |heap, thread| {
                match crate::builtins::continue_require_after_resolve(
                    heap,
                    thread,
                    source,
                    id.clone(),
                    requester,
                    &crate::builtins::RequireCallSite {
                        result_reg: site.result_reg,
                        result_count: site.result_count,
                        cleanup_end: site.cleanup_end,
                    },
                ) {
                    Ok(crate::builtins::RequireCallStep::WaitForInFlight(loading_key))
                        if root.allow_module_wait =>
                    {
                        Ok(DriverResume::WaitForModule(ModuleWait {
                            loading_key,
                            source: std::sync::Arc::clone(source),
                            id: id.clone(),
                            requester: requester.clone(),
                            site,
                            target: *target,
                        }))
                    }
                    Ok(step) => resume_active_require_step(
                        heap,
                        thread,
                        step,
                        site,
                        root.state,
                        root.traceback_limit,
                    ),
                    Err(error) => resume_active_require_error(
                        heap,
                        thread,
                        site,
                        error,
                        root.state,
                        root.traceback_limit,
                    ),
                }
            })
        }
        SuspendedTarget::Coroutine {
            thread: coroutine_thread,
            resume_result_reg,
            resume_result_count,
            resume_call_pc,
        } => {
            let coroutine_thread = *coroutine_thread;
            let resume_result_reg = *resume_result_reg;
            let resume_result_count = *resume_result_count;
            let resume_call_pc = *resume_call_pc;
            heap.drain_releases();
            let Some(mut co) = heap.take_thread(coroutine_thread) else {
                return Err(unwind_main(
                    heap,
                    root.main_thread,
                    root.state,
                    root.host_entry,
                    root.traceback_limit,
                    err("suspended coroutine is not resident"),
                ));
            };
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                crate::builtins::continue_require_after_resolve(
                    heap,
                    &mut co,
                    source,
                    id,
                    requester,
                    &crate::builtins::RequireCallSite {
                        result_reg: site.result_reg,
                        result_count: site.result_count,
                        cleanup_end: site.cleanup_end,
                    },
                )
            }));
            let restored = heap.put_thread(coroutine_thread, co);
            if !restored {
                return Err(DriverError::Poison);
            }
            match outcome {
                Ok(Ok(step)) => resume_coroutine_require_step(
                    heap,
                    coroutine_thread,
                    ResumeSite {
                        result_reg: resume_result_reg,
                        result_count: resume_result_count,
                        call_pc: resume_call_pc,
                    },
                    site,
                    step,
                    root,
                ),
                Ok(Err(error)) => resume_require_error(
                    heap,
                    &SuspendedTarget::Coroutine {
                        thread: coroutine_thread,
                        resume_result_reg,
                        resume_result_count,
                        resume_call_pc,
                    },
                    site,
                    root,
                    error,
                ),
                Err(_) => Err(DriverError::Poison),
            }
        }
    }
}

fn resume_read_require(
    heap: &mut Heap,
    target: &SuspendedTarget,
    read: RequireReadReady,
    site: AwaitSite,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    let RequireReadReady {
        id,
        instance,
        epoch,
        loading_key,
        source,
    } = read;
    match target {
        SuspendedTarget::Active => {
            with_thread_segment(heap, root.main_thread, root.host_entry, |heap, thread| {
                match crate::builtins::start_require_body(
                    heap,
                    thread,
                    crate::builtins::RequireBodyStart {
                        id,
                        instance,
                        epoch,
                        loading_key,
                    },
                    &source,
                    &crate::builtins::RequireCallSite {
                        result_reg: site.result_reg,
                        result_count: site.result_count,
                        cleanup_end: site.cleanup_end,
                    },
                ) {
                    Ok(()) => Ok(DriverResume::Continue),
                    Err(error) => resume_active_require_error(
                        heap,
                        thread,
                        site,
                        error,
                        root.state,
                        root.traceback_limit,
                    ),
                }
            })
        }
        SuspendedTarget::Coroutine {
            thread: coroutine_thread,
            resume_result_reg,
            resume_result_count,
            resume_call_pc,
        } => {
            let coroutine_thread = *coroutine_thread;
            let resume_result_reg = *resume_result_reg;
            let resume_result_count = *resume_result_count;
            let resume_call_pc = *resume_call_pc;
            heap.drain_releases();
            let Some(mut co) = heap.take_thread(coroutine_thread) else {
                return Err(unwind_main(
                    heap,
                    root.main_thread,
                    root.state,
                    root.host_entry,
                    root.traceback_limit,
                    err("suspended coroutine is not resident"),
                ));
            };
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                crate::builtins::start_require_body(
                    heap,
                    &mut co,
                    crate::builtins::RequireBodyStart {
                        id,
                        instance,
                        epoch,
                        loading_key,
                    },
                    &source,
                    &crate::builtins::RequireCallSite {
                        result_reg: site.result_reg,
                        result_count: site.result_count,
                        cleanup_end: site.cleanup_end,
                    },
                )
            }));
            let restored = heap.put_thread(coroutine_thread, co);
            if !restored {
                return Err(DriverError::Poison);
            }
            match outcome {
                Ok(Ok(())) => resume_coroutine_require_body(
                    heap,
                    coroutine_thread,
                    ResumeSite {
                        result_reg: resume_result_reg,
                        result_count: resume_result_count,
                        call_pc: resume_call_pc,
                    },
                    root,
                ),
                Ok(Err(error)) => resume_require_error(
                    heap,
                    &SuspendedTarget::Coroutine {
                        thread: coroutine_thread,
                        resume_result_reg,
                        resume_result_count,
                        resume_call_pc,
                    },
                    site,
                    root,
                    error,
                ),
                Err(_) => Err(DriverError::Poison),
            }
        }
    }
}

fn resume_active_require_step(
    heap: &mut Heap,
    thread: &mut Thread,
    step: crate::builtins::RequireCallStep,
    site: AwaitSite,
    state: ProtectedState,
    traceback_limit: Option<usize>,
) -> DriverExec<DriverResume> {
    match step {
        crate::builtins::RequireCallStep::Ready(results) => {
            resume_active_require_success(heap, thread, &results, site, state, traceback_limit)
        }
        crate::builtins::RequireCallStep::WaitForInFlight(_) => resume_active_require_error(
            heap,
            thread,
            site,
            err("required module is already loading"),
            state,
            traceback_limit,
        ),
        crate::builtins::RequireCallStep::Suspend(require) => {
            Ok(DriverResume::suspend(DriverSuspend::Require(require)))
        }
        crate::builtins::RequireCallStep::BodyStarted => Ok(DriverResume::Continue),
    }
}

fn resume_active_require_success(
    heap: &mut Heap,
    thread: &mut Thread,
    values: &[RawValue],
    site: AwaitSite,
    state: ProtectedState,
    traceback_limit: Option<usize>,
) -> DriverExec<DriverResume> {
    if let Err(error) = place_results(heap, thread, site.result_reg, site.result_count, values) {
        return resume_active_require_error(heap, thread, site, error, state, traceback_limit);
    }
    crate::call::clear_call_temps(thread, site.result_reg, values.len(), site.cleanup_end);
    Ok(DriverResume::Continue)
}

fn resume_active_require_error(
    heap: &mut Heap,
    thread: &mut Thread,
    site: AwaitSite,
    error: RaisedError,
    state: ProtectedState,
    traceback_limit: Option<usize>,
) -> DriverExec<DriverResume> {
    locate_at_call(thread, site.call_pc);
    Err(unwind_protected_failure(
        heap,
        thread,
        state.floor,
        state.saved_top,
        error,
        traceback_limit,
    )
    .into())
}

fn resume_require_error(
    heap: &mut Heap,
    target: &SuspendedTarget,
    site: AwaitSite,
    root: DriverRoot<'_>,
    error: RaisedError,
) -> DriverExec<DriverResume> {
    match target {
        SuspendedTarget::Active => {
            with_thread_segment(heap, root.main_thread, root.host_entry, |heap, thread| {
                resume_active_require_error(
                    heap,
                    thread,
                    site,
                    error,
                    root.state,
                    root.traceback_limit,
                )
            })
        }
        SuspendedTarget::Coroutine {
            thread: coroutine_thread,
            resume_result_reg,
            resume_result_count,
            resume_call_pc,
        } => resume_coroutine_require_error(
            heap,
            *coroutine_thread,
            ResumeSite {
                result_reg: *resume_result_reg,
                result_count: *resume_result_count,
                call_pc: *resume_call_pc,
            },
            site,
            error,
            root,
        ),
    }
}

fn resume_coroutine_require_step(
    heap: &mut Heap,
    coroutine_thread: RawGc<marker::Thread>,
    resume_site: ResumeSite,
    require_site: AwaitSite,
    step: crate::builtins::RequireCallStep,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    match step {
        crate::builtins::RequireCallStep::Ready(values) => resume_coroutine_require_values(
            heap,
            coroutine_thread,
            resume_site,
            require_site,
            &values,
            root,
        ),
        crate::builtins::RequireCallStep::WaitForInFlight(loading_key) => {
            suspend_coroutine_require_wait(
                heap,
                coroutine_thread,
                resume_site,
                require_site,
                loading_key,
                root,
            )
        }
        crate::builtins::RequireCallStep::Suspend(mut next) => {
            next.target = SuspendedTarget::Coroutine {
                thread: coroutine_thread,
                resume_result_reg: resume_site.result_reg,
                resume_result_count: resume_site.result_count,
                resume_call_pc: resume_site.call_pc,
            };
            Ok(DriverResume::suspend(DriverSuspend::Require(next)))
        }
        crate::builtins::RequireCallStep::BodyStarted => {
            resume_coroutine_require_body(heap, coroutine_thread, resume_site, root)
        }
    }
}

fn suspend_coroutine_require_wait(
    heap: &mut Heap,
    coroutine_thread: RawGc<marker::Thread>,
    resume_site: ResumeSite,
    require_site: AwaitSite,
    loading_key: crate::heap::ModuleCacheKey,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    heap.drain_releases();
    let Some(mut co) = heap.take_thread(coroutine_thread) else {
        return Err(unwind_main(
            heap,
            root.main_thread,
            root.state,
            root.host_entry,
            root.traceback_limit,
            err("suspended coroutine is not resident"),
        ));
    };
    if let Some(frame) = co
        .call_stack
        .iter_mut()
        .rev()
        .find_map(CallStackEntry::frame_mut)
    {
        frame.savedpc = require_site.call_pc;
    }
    co.status = CoroutineStatus::Suspended;
    let restored = heap.put_thread(coroutine_thread, co);
    if !restored {
        return Err(DriverError::Poison);
    }
    let step = if heap.module_load_owned_by_current(&loading_key) {
        CoroutineStep::Values(vec![RawValue::Boolean(true)])
    } else {
        CoroutineStep::WaitForModule(loading_key)
    };
    resume_coroutine_step(heap, resume_site, step, coroutine_thread, root)
}

/// Takes the parked coroutine out of the heap, runs `f` on it behind the
/// host-call panic guard, restores it, and hands the produced step to
/// [`resume_coroutine_step`]. A missing thread unwinds the main thread, a
/// raised error unwinds it with that error, and a panic or failed restore
/// poisons the driver — the shared frame of every post-await require resume.
fn resume_parked_coroutine(
    heap: &mut Heap,
    coroutine_thread: RawGc<marker::Thread>,
    resume_site: ResumeSite,
    root: DriverRoot<'_>,
    f: impl FnOnce(&mut Heap, &mut Thread) -> Exec<CoroutineStep>,
) -> DriverExec<DriverResume> {
    heap.drain_releases();
    let Some(mut co) = heap.take_thread(coroutine_thread) else {
        return Err(unwind_main(
            heap,
            root.main_thread,
            root.state,
            root.host_entry,
            root.traceback_limit,
            err("suspended coroutine is not resident"),
        ));
    };
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| f(heap, &mut co)));
    let restored = heap.put_thread(coroutine_thread, co);
    if !restored {
        return Err(DriverError::Poison);
    }
    let step = match outcome {
        Ok(Ok(step)) => step,
        Ok(Err(error)) => {
            return Err(unwind_main(
                heap,
                root.main_thread,
                root.state,
                root.host_entry,
                root.traceback_limit,
                error,
            ));
        }
        Err(_) => return Err(DriverError::Poison),
    };
    resume_coroutine_step(heap, resume_site, step, coroutine_thread, root)
}

fn resume_coroutine_require_body(
    heap: &mut Heap,
    coroutine_thread: RawGc<marker::Thread>,
    resume_site: ResumeSite,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    resume_parked_coroutine(heap, coroutine_thread, resume_site, root, |heap, co| {
        crate::coroutine::continue_body_step(heap, co, false, root.host_entry)
    })
}

fn resume_coroutine_require_values(
    heap: &mut Heap,
    coroutine_thread: RawGc<marker::Thread>,
    resume_site: ResumeSite,
    require_site: AwaitSite,
    values: &[RawValue],
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    resume_parked_coroutine(heap, coroutine_thread, resume_site, root, |heap, co| {
        crate::coroutine::resume_after_async_success(
            heap,
            co,
            require_site.call_pc,
            require_site.result_reg,
            require_site.result_count,
            require_site.cleanup_end,
            values,
            root.host_entry,
        )
    })
}

fn resume_coroutine_require_error(
    heap: &mut Heap,
    coroutine_thread: RawGc<marker::Thread>,
    resume_site: ResumeSite,
    require_site: AwaitSite,
    error: RaisedError,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    resume_parked_coroutine(heap, coroutine_thread, resume_site, root, |heap, co| {
        crate::coroutine::resume_after_async_error(
            heap,
            co,
            require_site.call_pc,
            error,
            root.host_entry,
        )
    })
}

fn resume_coroutine_step(
    heap: &mut Heap,
    resume_site: ResumeSite,
    step: CoroutineStep,
    coroutine_thread: RawGc<marker::Thread>,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    match step {
        CoroutineStep::Values(values) => place_coroutine_resume_results(
            heap,
            root.main_thread,
            root.state,
            root.host_entry,
            root.traceback_limit,
            resume_site,
            &values,
        ),
        CoroutineStep::Suspend(mut next) => {
            next.target = SuspendedTarget::Coroutine {
                thread: coroutine_thread,
                resume_result_reg: resume_site.result_reg,
                resume_result_count: resume_site.result_count,
                resume_call_pc: resume_site.call_pc,
            };
            Ok(DriverResume::suspend(DriverSuspend::Call(next)))
        }
        CoroutineStep::SuspendRequire(mut next) => {
            next.target = SuspendedTarget::Coroutine {
                thread: coroutine_thread,
                resume_result_reg: resume_site.result_reg,
                resume_result_count: resume_site.result_count,
                resume_call_pc: resume_site.call_pc,
            };
            Ok(DriverResume::suspend(DriverSuspend::Require(next)))
        }
        CoroutineStep::WaitForModule(loading_key) if root.allow_module_wait => {
            Ok(DriverResume::WaitForCoroutine(CoroutineWait {
                loading_key,
                coroutine_thread,
                resume_site,
            }))
        }
        CoroutineStep::WaitForModule(_) => Err(unwind_main(
            heap,
            root.main_thread,
            root.state,
            root.host_entry,
            root.traceback_limit,
            err("required module is already loading"),
        )),
        CoroutineStep::Preempt => {
            unreachable!(
                "post-await coroutine resumes are non-preemptible until the resumer chain is rooted"
            )
        }
    }
}

fn resume_waiting_coroutine(
    heap: &mut Heap,
    wait: &CoroutineWait,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    resume_parked_coroutine(
        heap,
        wait.coroutine_thread,
        wait.resume_site,
        root,
        |heap, co| crate::coroutine::continue_body_step(heap, co, false, root.host_entry),
    )
}

fn place_coroutine_resume_results(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    state: ProtectedState,
    host_entry: HostEntry<'_>,
    traceback_limit: Option<usize>,
    site: ResumeSite,
    values: &[RawValue],
) -> DriverExec<DriverResume> {
    with_thread_segment(heap, main_thread, host_entry, |heap, thread| {
        if let Err(error) = place_results(heap, thread, site.result_reg, site.result_count, values)
        {
            locate_at_call(thread, site.call_pc);
            return Err(unwind_protected_failure(
                heap,
                thread,
                state.floor,
                state.saved_top,
                error,
                traceback_limit,
            )
            .into());
        }
        Ok(DriverResume::Continue)
    })
}

fn unwind_main(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    state: ProtectedState,
    host_entry: HostEntry<'_>,
    traceback_limit: Option<usize>,
    error: RaisedError,
) -> DriverError {
    match with_thread_segment(heap, main_thread, host_entry, |heap, thread| {
        Ok(unwind_protected_failure(
            heap,
            thread,
            state.floor,
            state.saved_top,
            error,
            traceback_limit,
        ))
    }) {
        Ok(failure) => DriverError::Runtime(failure),
        Err(error) => error,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the async resume boundary keeps the explicit host entry separate"
)]
fn resume_active_await(
    heap: &mut Heap,
    thread: &mut Thread,
    mut pins: Vec<crate::api::RegistryRef>,
    site: AwaitSite,
    outcome: Result<HostReturn, AwaitFailure>,
    state: ProtectedState,
    traceback_limit: Option<usize>,
    host_entry: HostEntry<'_>,
) -> DriverExec<DriverResume> {
    let host_return = match outcome {
        Ok(host_return) => host_return,
        Err(failure) => {
            release_pins(heap, &mut pins);
            locate_at_call(thread, site.call_pc);
            return resume_active_error(
                heap,
                thread,
                state,
                traceback_limit,
                await_failure_error(failure),
                host_entry,
            );
        }
    };
    let materialized = materialize_return(heap, &host_return, site.result_count);
    release_pins(heap, &mut pins);
    let values = match materialized {
        Ok(values) => values,
        Err(error) => {
            locate_at_call(thread, site.call_pc);
            return resume_active_error(heap, thread, state, traceback_limit, error, host_entry);
        }
    };
    if let Err(error) = place_results(heap, thread, site.result_reg, site.result_count, &values) {
        locate_at_call(thread, site.call_pc);
        return resume_active_error(heap, thread, state, traceback_limit, error, host_entry);
    }
    crate::call::clear_call_temps(thread, site.result_reg, values.len(), site.cleanup_end);
    Ok(DriverResume::Continue)
}

fn resume_active_error(
    heap: &mut Heap,
    thread: &mut Thread,
    state: ProtectedState,
    traceback_limit: Option<usize>,
    error: RaisedError,
    host_entry: HostEntry<'_>,
) -> DriverExec<DriverResume> {
    match catch_protected_error(heap, thread, state.floor, error, host_entry) {
        Ok(()) => Ok(DriverResume::Continue),
        Err(error) => Err(unwind_protected_failure(
            heap,
            thread,
            state.floor,
            state.saved_top,
            error,
            traceback_limit,
        )
        .into()),
    }
}

fn resume_coroutine_await(
    heap: &mut Heap,
    co: &mut Thread,
    mut pins: Vec<crate::api::RegistryRef>,
    site: AwaitSite,
    outcome: Result<HostReturn, AwaitFailure>,
    host_entry: HostEntry<'_>,
) -> Exec<CoroutineStep> {
    let host_return = match outcome {
        Ok(host_return) => host_return,
        Err(failure) => {
            release_pins(heap, &mut pins);
            return coroutine::resume_after_async_error(
                heap,
                co,
                site.call_pc,
                await_failure_error(failure),
                host_entry,
            );
        }
    };
    let materialized = materialize_return(heap, &host_return, site.result_count);
    release_pins(heap, &mut pins);
    let values = match materialized {
        Ok(values) => values,
        Err(error) => {
            return coroutine::resume_after_async_error(heap, co, site.call_pc, error, host_entry);
        }
    };
    coroutine::resume_after_async_success(
        heap,
        co,
        site.call_pc,
        site.result_reg,
        site.result_count,
        site.cleanup_end,
        &values,
        host_entry,
    )
}

fn release_pins(heap: &mut Heap, pins: &mut Vec<crate::api::RegistryRef>) {
    for reference in pins.drain(..) {
        heap.unpin(&reference);
    }
}

/// Why an awaited host call did not deliver a result: the host itself failed, or
/// the request's deadline or cancellation tripped first. The latter two become
/// fatal (uncatchable) runtime errors so a tenant cannot swallow them.
enum AwaitFailure {
    /// The host future resolved to an error.
    Host(HostError),
    /// The wall-clock deadline passed while the future was pending.
    Stopped(StopReason),
}

fn await_failure_error(failure: AwaitFailure) -> RaisedError {
    match failure {
        AwaitFailure::Host(host_error) => RaisedError {
            payload: error_payload_from_message(host_error.message, host_error.script_fields),
            located: false,
            location_level: 1,
            kind: host_error.kind,
            host_payload: host_error.payload,
        },
        AwaitFailure::Stopped(reason) => err_stopped(reason),
    }
}

/// Awaits a pending host future under the request's governance: it resolves to
/// the host's result, or to a deadline / cancellation failure if either trips
/// first. This is the production driver's `select!`; the deterministic
/// model drives `dispatch`/resume directly with scripted completions instead.
async fn await_governed(
    mut future: crate::api::HostFuture,
    mut host_requests: Option<HostRequests>,
    governance: &Governance,
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    scope_thread: RawGc<marker::Thread>,
    host_entry: HostEntry<'_>,
) -> DriverExec<Result<HostReturn, AwaitFailure>> {
    loop {
        let Some(requests) = host_requests.as_mut() else {
            return Ok(tokio::select! {
                biased;
                result = &mut future => result.map_err(AwaitFailure::Host),
                () = deadline_elapsed(governance.deadline) => Err(AwaitFailure::Stopped(stop_for_deadline(governance))),
                () = cancellation(governance.cancel.as_ref()) => Err(AwaitFailure::Stopped(stop_for_cancel(governance.cancel.as_ref()))),
            });
        };
        tokio::select! {
            // Bias toward the future: a future already ready when the deadline has
            // also passed still delivers its result rather than spuriously timing out.
            biased;
            result = &mut future => return Ok(result.map_err(AwaitFailure::Host)),
            () = deadline_elapsed(governance.deadline) => {
                return Ok(Err(AwaitFailure::Stopped(stop_for_deadline(governance))));
            }
            () = cancellation(governance.cancel.as_ref()) => {
                return Ok(Err(AwaitFailure::Stopped(stop_for_cancel(governance.cancel.as_ref()))));
            }
            request = requests.recv() => {
                if let Some(request) = request {
                    service_host_request(
                        heap,
                        main_thread,
                        scope_thread,
                        governance,
                        host_entry,
                        request,
                    )
                    .await?;
                } else {
                    host_requests = None;
                }
            }
        }
    }
}

async fn service_host_request(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    scope_thread: RawGc<marker::Thread>,
    governance: &Governance,
    host_entry: HostEntry<'_>,
    request: HostRequest,
) -> DriverExec<()> {
    match request {
        HostRequest::Scope(request) => {
            service_scope_request(heap, scope_thread, host_entry, request)
        }
        HostRequest::ProtectedCall(request) => {
            service_protected_call_request(
                heap,
                main_thread,
                scope_thread,
                governance,
                host_entry,
                request,
            )
            .await
        }
    }
}

fn service_scope_request(
    heap: &mut Heap,
    scope_thread: RawGc<marker::Thread>,
    host_entry: HostEntry<'_>,
    request: HostScopeRequest,
) -> DriverExec<()> {
    with_thread_segment(heap, scope_thread, host_entry, |heap, thread| {
        let scope = Scope::for_host_call(heap, thread, host_entry);
        request.run(&scope);
        Ok(())
    })
}

struct PreparedHostProtectedCall {
    callback_thread: RawGc<marker::Thread>,
    callback: RawValue,
    args: Vec<RawValue>,
    roots: Vec<RegistryRef>,
}

async fn service_protected_call_request(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    scope_thread: RawGc<marker::Thread>,
    governance: &Governance,
    host_entry: HostEntry<'_>,
    request: HostProtectedCallRequest,
) -> DriverExec<()> {
    let HostProtectedCallRequest {
        callback,
        convert_args,
        reply,
    } = request;
    let prepared = prepare_host_protected_call(
        heap,
        main_thread,
        scope_thread,
        host_entry,
        &callback,
        convert_args,
    )?;
    match prepared {
        Ok(prepared) => {
            let result = run_host_protected_call(heap, governance, host_entry, prepared).await;
            drop(reply.send(result));
        }
        Err(error) => {
            drop(reply.send(Err(error)));
        }
    }
    Ok(())
}

fn prepare_host_protected_call(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    scope_thread: RawGc<marker::Thread>,
    host_entry: HostEntry<'_>,
    callback: &crate::scope::Stashed<marker::Closure>,
    convert_args: ProtectedArgsOperation,
) -> DriverExec<Result<PreparedHostProtectedCall, RuntimeError>> {
    with_thread_segment(heap, scope_thread, host_entry, |heap, thread| {
        // Each re-entry level is a native recursion through the driver: resolving
        // the nested protected run's future polls through every enclosing level's
        // poll frame. Charge `max_native_depth` per level — seeded from the
        // suspended thread so recursive re-entries accumulate — and fail closed
        // with a catchable error so an unbounded predicate recursion unwinds
        // cleanly instead of exhausting the Rust stack.
        let reentry_depth = thread.native_depth.saturating_add(1);
        if reentry_depth > heap.limits().max_native_depth {
            return Ok(Err(RuntimeError::runtime(
                "stack overflow (async host re-entry)",
            )));
        }
        let raw_callback = match heap.pinned_value(callback.reference()) {
            Ok(raw @ RawValue::Function(_)) => raw,
            Ok(_) => {
                return Ok(Err(RuntimeError::runtime(
                    "stashed value is not a function",
                )));
            }
            Err(message) => return Ok(Err(RuntimeError::runtime(message))),
        };
        let scope = Scope::for_host_call(heap, thread, host_entry);
        let args = match convert_args(&scope) {
            Ok(args) => args.into_raw(),
            Err(error) => return Ok(Err(error)),
        };
        let mut roots = match scope.pin_raw_values(&args) {
            Ok(roots) => roots,
            Err(error) => return Ok(Err(error)),
        };
        drop(scope);

        let mut callback_thread = Thread::new();
        callback_thread.globals = thread.globals;
        callback_thread.native_depth = reentry_depth;
        callback_thread.base_native_depth = reentry_depth;
        let Some(callback_thread_handle) = heap.alloc_thread(callback_thread) else {
            unpin_roots(heap, &mut roots);
            return Ok(Err(RuntimeError::memory(
                "out of memory creating an async host callback thread",
            )));
        };
        if let Some(callback_thread) = heap.thread_mut(callback_thread_handle) {
            callback_thread.id = Some(callback_thread_handle);
        }

        for root in [main_thread, scope_thread, callback_thread_handle] {
            match heap.pin(RawValue::Thread(root)) {
                Some(reference) => roots.push(reference),
                None => {
                    unpin_roots(heap, &mut roots);
                    return Ok(Err(RuntimeError::memory(
                        "out of memory rooting async host callback state",
                    )));
                }
            }
        }

        Ok(Ok(PreparedHostProtectedCall {
            callback_thread: callback_thread_handle,
            callback: raw_callback,
            args,
            roots,
        }))
    })
}

async fn run_host_protected_call(
    heap: &mut Heap,
    governance: &Governance,
    host_entry: HostEntry<'_>,
    mut prepared: PreparedHostProtectedCall,
) -> Result<Result<HostReturn, HostScriptError>, RuntimeError> {
    let outcome = Box::pin(run_async_function_protected(
        heap,
        prepared.callback_thread,
        prepared.callback,
        std::mem::take(&mut prepared.args),
        governance,
        host_entry,
        crate::SCRIPT_ERROR_TRACEBACK_MAX_BYTES,
    ))
    .await;
    let converted = match outcome {
        Ok(Ok(values)) => {
            owned_values_from_raw(heap, &values).map(|values| Ok(HostReturn { values }))
        }
        Ok(Err(failure)) => {
            let capture = heap
                .thread_mut(prepared.callback_thread)
                .and_then(|thread| thread.captured_traceback.take());
            owned_script_error_from_failure(heap, failure, capture).map(Err)
        }
        Err(error) => Err(error_from_host_protected_unwind(&error)),
    };
    unpin_roots(heap, &mut prepared.roots);
    converted
}

fn unpin_roots(heap: &mut Heap, roots: &mut Vec<RegistryRef>) {
    for reference in roots.drain(..) {
        heap.unpin(&reference);
    }
}

fn owned_script_error_from_failure(
    heap: &mut Heap,
    failure: ProtectedFailure,
    capture: Option<crate::debug::Traceback>,
) -> Result<HostScriptError, RuntimeError> {
    let kind = failure.error.kind;
    let traceback = failure.traceback;
    let raw = materialize(heap, failure.error);
    let value = owned_value_from_raw(heap, raw)?;
    Ok(HostScriptError::new(value, kind, traceback, capture))
}

fn owned_values_from_raw(
    heap: &mut Heap,
    values: &[RawValue],
) -> Result<Vec<OwnedValue>, RuntimeError> {
    let mut owned = Vec::new();
    owned
        .try_reserve(values.len())
        .map_err(|_| RuntimeError::memory("out of memory owning result values"))?;
    for &value in values {
        owned.push(owned_value_from_raw(heap, value)?);
    }
    Ok(owned)
}

fn owned_value_from_raw(heap: &mut Heap, value: RawValue) -> Result<OwnedValue, RuntimeError> {
    match value {
        RawValue::Nil => Ok(OwnedValue::Nil),
        RawValue::Boolean(value) => Ok(OwnedValue::Boolean(value)),
        RawValue::Number(value) => Ok(OwnedValue::Number(value)),
        RawValue::Integer(value) => Ok(OwnedValue::Integer(value)),
        RawValue::Vector(value) => Ok(OwnedValue::Vector(value)),
        RawValue::LightUserdata { handle, tag } => Ok(OwnedValue::LightUserdata { handle, tag }),
        RawValue::String(handle) => heap
            .string(handle)
            .map(|string| OwnedValue::Bytes(string.bytes().to_vec()))
            .ok_or_else(|| RuntimeError::runtime("string handle no longer resolves")),
        RawValue::Table(_)
        | RawValue::Function(_)
        | RawValue::Userdata(_)
        | RawValue::Thread(_)
        | RawValue::Buffer(_) => heap
            .pin(value)
            .map(OwnedValue::Pinned)
            .ok_or_else(|| RuntimeError::memory("out of memory owning heap result value")),
    }
}

fn error_from_host_protected_unwind(error: &Unwind) -> RuntimeError {
    let message = match error.kind {
        RuntimeErrorKind::Cancelled => "AsyncHostContext::call_protected: callback was cancelled",
        RuntimeErrorKind::Deadline => {
            "AsyncHostContext::call_protected: callback exceeded its deadline"
        }
        RuntimeErrorKind::PanicPoison => {
            "AsyncHostContext::call_protected: callback poisoned the VM and refuses further work"
        }
        RuntimeErrorKind::Memory => {
            "AsyncHostContext::call_protected: callback exceeded the memory cap"
        }
        _ => "AsyncHostContext::call_protected: an uncatchable callback error escaped",
    };
    RuntimeError::with_kind(message, error.kind)
}

pub struct DetachedDriver {
    invocation: u64,
    frames: Vec<DetachedFrame>,
}

pub struct DetachedReady {
    pub(crate) outcome: Result<Result<Vec<RawValue>, ProtectedFailure>, Unwind>,
    pub(crate) main_thread: RawGc<marker::Thread>,
    pub(crate) root: RegistryRef,
}

struct DetachedFrame {
    main_thread: RawGc<marker::Thread>,
    state: ProtectedState,
    traceback_limit: Option<usize>,
    phase: DetachedPhase,
    completion: DetachedCompletion,
}

enum DetachedCompletion {
    Root(RegistryRef),
    Host {
        prepared: PreparedHostProtectedCall,
        reply: tokio::sync::oneshot::Sender<HostProtectedCallResult>,
    },
}

enum DetachedPhase {
    Dispatch,
    Call(DetachedCall),
    Require(SuspendedRequire),
    DispatchWait(crate::heap::ModuleCacheKey),
    WaitForModule(ModuleWait),
    WaitForCoroutine(CoroutineWait),
}

struct DetachedCall {
    future: crate::api::HostFuture,
    host_requests: Option<HostRequests>,
    pins: Vec<RegistryRef>,
    site: AwaitSite,
    target: SuspendedTarget,
}

enum DetachedFramePoll {
    Pending,
    Push(Box<DetachedFrame>),
    Ready(DriverExec<Vec<RawValue>>),
}

impl DetachedDriver {
    pub(crate) fn start_root(
        heap: &mut Heap,
        shared_main_thread: RawGc<marker::Thread>,
        main: RawGc<marker::Closure>,
        invocation: u64,
        host_entry: HostEntry<'_>,
    ) -> Result<Self, RuntimeError> {
        let globals = heap
            .closure(main)
            .and_then(|closure| closure.env)
            .or_else(|| {
                heap.thread(shared_main_thread)
                    .and_then(|thread| thread.globals)
            })
            .ok_or_else(|| RuntimeError::runtime("detached root has no globals table"))?;
        let (main_thread, root) = allocate_detached_thread(heap, globals)?;
        let state = match start_protected_async(heap, main_thread, host_entry, |heap, thread| {
            root_frame(heap, thread, main)
        }) {
            Ok(state) => state,
            Err(error) => {
                heap.unpin(&root);
                return Err(detached_start_error(error));
            }
        };
        Ok(Self {
            invocation,
            frames: vec![DetachedFrame {
                main_thread,
                state,
                traceback_limit: Some(crate::SCRIPT_ERROR_TRACEBACK_MAX_BYTES),
                phase: DetachedPhase::Dispatch,
                completion: DetachedCompletion::Root(root),
            }],
        })
    }

    pub(crate) fn start_function(
        heap: &mut Heap,
        shared_main_thread: RawGc<marker::Thread>,
        function: RawValue,
        args: Vec<RawValue>,
        invocation: u64,
        host_entry: HostEntry<'_>,
    ) -> Result<Self, RuntimeError> {
        let globals = match function {
            RawValue::Function(function) => heap.closure(function).and_then(|closure| closure.env),
            _ => None,
        }
        .or_else(|| {
            heap.thread(shared_main_thread)
                .and_then(|thread| thread.globals)
        })
        .ok_or_else(|| RuntimeError::runtime("detached function has no globals table"))?;
        let (main_thread, root) = allocate_detached_thread(heap, globals)?;
        let state = match start_protected_async(heap, main_thread, host_entry, |heap, thread| {
            root_function_frame(heap, thread, function, &args)
        }) {
            Ok(state) => state,
            Err(error) => {
                heap.unpin(&root);
                return Err(detached_start_error(error));
            }
        };
        drop(args);
        Ok(Self {
            invocation,
            frames: vec![DetachedFrame {
                main_thread,
                state,
                traceback_limit: Some(crate::SCRIPT_ERROR_TRACEBACK_MAX_BYTES),
                phase: DetachedPhase::Dispatch,
                completion: DetachedCompletion::Root(root),
            }],
        })
    }

    pub(crate) fn poll(
        &mut self,
        heap: &mut Heap,
        host_entry: HostEntry<'_>,
        deadline: Option<Instant>,
        context: &mut Context<'_>,
    ) -> Poll<DetachedReady> {
        loop {
            let mut frame = self
                .frames
                .pop()
                .expect("detached driver always retains its root frame");
            match frame.poll(heap, host_entry, deadline, context) {
                DetachedFramePoll::Pending => {
                    self.frames.push(frame);
                    return Poll::Pending;
                }
                DetachedFramePoll::Push(child) => {
                    self.frames.push(frame);
                    self.frames.push(*child);
                }
                DetachedFramePoll::Ready(outcome) => match frame.completion {
                    DetachedCompletion::Root(root) => {
                        let main_thread = frame.main_thread;
                        let outcome = protect_driver_outcome(heap, outcome);
                        return Poll::Ready(DetachedReady {
                            outcome,
                            main_thread,
                            root,
                        });
                    }
                    DetachedCompletion::Host {
                        mut prepared,
                        reply,
                    } => {
                        if matches!(outcome, Err(DriverError::Poison)) {
                            let (main_thread, root) = self
                                .root_completion()
                                .expect("nested detached frame keeps a root frame");
                            return Poll::Ready(DetachedReady {
                                outcome: Err(panic_poison_unwind()),
                                main_thread,
                                root,
                            });
                        }
                        let converted = finish_detached_host_call(heap, frame.main_thread, outcome);
                        unpin_roots(heap, &mut prepared.roots);
                        drop(reply.send(converted));
                    }
                },
            }
        }
    }

    pub(crate) fn abort(mut self, heap: &mut Heap, host_entry: HostEntry<'_>) {
        while let Some(mut frame) = self.frames.pop() {
            frame.release_pending(heap);
            drop(unwind_main(
                heap,
                frame.main_thread,
                frame.state,
                host_entry,
                frame.traceback_limit,
                err("detached invocation aborted"),
            ));
            match frame.completion {
                DetachedCompletion::Root(root) => heap.unpin(&root),
                DetachedCompletion::Host {
                    mut prepared,
                    reply,
                } => {
                    unpin_roots(heap, &mut prepared.roots);
                    drop(reply);
                }
            }
        }
        heap.abort_detached_invocation_coroutines(self.invocation);
    }

    fn root_completion(&self) -> Option<(RawGc<marker::Thread>, RegistryRef)> {
        self.frames
            .iter()
            .find_map(|frame| match &frame.completion {
                DetachedCompletion::Root(root) => Some((frame.main_thread, root.clone())),
                DetachedCompletion::Host { .. } => None,
            })
    }
}

impl DetachedFrame {
    fn poll(
        &mut self,
        heap: &mut Heap,
        host_entry: HostEntry<'_>,
        deadline: Option<Instant>,
        context: &mut Context<'_>,
    ) -> DetachedFramePoll {
        loop {
            let phase = std::mem::replace(&mut self.phase, DetachedPhase::Dispatch);
            match phase {
                DetachedPhase::Dispatch => {
                    let step = dispatch_root_segment(
                        heap,
                        DriverRoot {
                            main_thread: self.main_thread,
                            state: self.state,
                            host_entry,
                            traceback_limit: self.traceback_limit,
                            allow_module_wait: true,
                        },
                    );
                    match step {
                        Err(error) => return DetachedFramePoll::Ready(Err(error)),
                        Ok(RootStep::Return(values)) => {
                            return DetachedFramePoll::Ready(Ok(values));
                        }
                        Ok(RootStep::Preempt) => {
                            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                                return DetachedFramePoll::Ready(Err(unwind_main(
                                    heap,
                                    self.main_thread,
                                    self.state,
                                    host_entry,
                                    self.traceback_limit,
                                    err_deadline("deadline exceeded at a preemption checkpoint"),
                                )));
                            }
                            self.phase = DetachedPhase::Dispatch;
                            context.waker().wake_by_ref();
                            return DetachedFramePoll::Pending;
                        }
                        Ok(RootStep::Suspend(call)) => {
                            self.phase = DetachedPhase::Call(DetachedCall::from(call));
                        }
                        Ok(RootStep::SuspendRequire(require)) => {
                            self.phase = DetachedPhase::Require(require);
                        }
                        Ok(RootStep::WaitForModule(loading_key)) => {
                            self.phase = DetachedPhase::DispatchWait(loading_key);
                        }
                    }
                }
                DetachedPhase::Call(mut call) => match call.future.as_mut().poll(context) {
                    Poll::Ready(result) => {
                        let resumed = resume_awaited_call_result(
                            heap,
                            self.main_thread,
                            call.pins,
                            call.site,
                            call.target,
                            result.map_err(AwaitFailure::Host),
                            self.state,
                            host_entry,
                            self.traceback_limit,
                            true,
                        );
                        match detached_phase_from_resume(resumed) {
                            Ok(phase) => self.phase = phase,
                            Err(error) => return DetachedFramePoll::Ready(Err(error)),
                        }
                    }
                    Poll::Pending => {
                        let Some(requests) = call.host_requests.as_mut() else {
                            self.phase = DetachedPhase::Call(call);
                            return DetachedFramePoll::Pending;
                        };
                        match Pin::new(requests).poll_recv(context) {
                            Poll::Pending => {
                                self.phase = DetachedPhase::Call(call);
                                return DetachedFramePoll::Pending;
                            }
                            Poll::Ready(None) => {
                                call.host_requests = None;
                                self.phase = DetachedPhase::Call(call);
                            }
                            Poll::Ready(Some(HostRequest::Scope(request))) => {
                                let scope_thread = call.scope_thread(self.main_thread);
                                match service_scope_request(heap, scope_thread, host_entry, request)
                                {
                                    Ok(()) => self.phase = DetachedPhase::Call(call),
                                    Err(error) => {
                                        return DetachedFramePoll::Ready(Err(error));
                                    }
                                }
                            }
                            Poll::Ready(Some(HostRequest::ProtectedCall(request))) => {
                                let scope_thread = call.scope_thread(self.main_thread);
                                let HostProtectedCallRequest {
                                    callback,
                                    convert_args,
                                    reply,
                                } = request;
                                let prepared = prepare_host_protected_call(
                                    heap,
                                    self.main_thread,
                                    scope_thread,
                                    host_entry,
                                    &callback,
                                    convert_args,
                                );
                                match prepared {
                                    Err(error) => {
                                        return DetachedFramePoll::Ready(Err(error));
                                    }
                                    Ok(Err(error)) => {
                                        drop(reply.send(Err(error)));
                                        self.phase = DetachedPhase::Call(call);
                                    }
                                    Ok(Ok(mut prepared)) => {
                                        let args = std::mem::take(&mut prepared.args);
                                        let state = start_protected_async(
                                            heap,
                                            prepared.callback_thread,
                                            host_entry,
                                            |heap, thread| {
                                                root_function_frame(
                                                    heap,
                                                    thread,
                                                    prepared.callback,
                                                    &args,
                                                )
                                            },
                                        );
                                        drop(args);
                                        match state {
                                            Ok(state) => {
                                                self.phase = DetachedPhase::Call(call);
                                                return DetachedFramePoll::Push(Box::new(Self {
                                                    main_thread: prepared.callback_thread,
                                                    state,
                                                    traceback_limit: Some(
                                                        crate::SCRIPT_ERROR_TRACEBACK_MAX_BYTES,
                                                    ),
                                                    phase: DetachedPhase::Dispatch,
                                                    completion: DetachedCompletion::Host {
                                                        prepared,
                                                        reply,
                                                    },
                                                }));
                                            }
                                            Err(error) => {
                                                unpin_roots(heap, &mut prepared.roots);
                                                drop(reply.send(Err(detached_start_error(error))));
                                                self.phase = DetachedPhase::Call(call);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                DetachedPhase::Require(mut require) => {
                    let ready = match &mut require.stage {
                        SuspendedRequireStage::Resolve { future, .. } => {
                            future.as_mut().poll(context).map(DetachedSourceReady::Id)
                        }
                        SuspendedRequireStage::Read { future, .. } => future
                            .as_mut()
                            .poll(context)
                            .map(DetachedSourceReady::Source),
                    };
                    let Poll::Ready(ready) = ready else {
                        self.phase = DetachedPhase::Require(require);
                        return DetachedFramePoll::Pending;
                    };
                    let root = DriverRoot {
                        main_thread: self.main_thread,
                        state: self.state,
                        host_entry,
                        traceback_limit: self.traceback_limit,
                        allow_module_wait: true,
                    };
                    let resumed = resume_detached_require(heap, require, ready, root);
                    match detached_phase_from_resume(resumed) {
                        Ok(phase) => self.phase = phase,
                        Err(error) => return DetachedFramePoll::Ready(Err(error)),
                    }
                }
                DetachedPhase::WaitForModule(wait) => {
                    if heap
                        .poll_module_load(&wait.loading_key, context)
                        .is_pending()
                    {
                        self.phase = DetachedPhase::WaitForModule(wait);
                        return DetachedFramePoll::Pending;
                    }
                    let root = DriverRoot {
                        main_thread: self.main_thread,
                        state: self.state,
                        host_entry,
                        traceback_limit: self.traceback_limit,
                        allow_module_wait: true,
                    };
                    let resumed = resume_resolved_require(
                        heap,
                        &wait.target,
                        &wait.source,
                        wait.id,
                        &wait.requester,
                        wait.site,
                        root,
                    );
                    match detached_phase_from_resume(resumed) {
                        Ok(phase) => self.phase = phase,
                        Err(error) => return DetachedFramePoll::Ready(Err(error)),
                    }
                }
                DetachedPhase::WaitForCoroutine(wait) => {
                    if heap
                        .poll_module_load(&wait.loading_key, context)
                        .is_pending()
                    {
                        self.phase = DetachedPhase::WaitForCoroutine(wait);
                        return DetachedFramePoll::Pending;
                    }
                    let root = DriverRoot {
                        main_thread: self.main_thread,
                        state: self.state,
                        host_entry,
                        traceback_limit: self.traceback_limit,
                        allow_module_wait: true,
                    };
                    let resumed = resume_waiting_coroutine(heap, &wait, root);
                    match detached_phase_from_resume(resumed) {
                        Ok(phase) => self.phase = phase,
                        Err(error) => return DetachedFramePoll::Ready(Err(error)),
                    }
                }
                DetachedPhase::DispatchWait(loading_key) => {
                    if heap.poll_module_load(&loading_key, context).is_pending() {
                        self.phase = DetachedPhase::DispatchWait(loading_key);
                        return DetachedFramePoll::Pending;
                    }
                    self.phase = DetachedPhase::Dispatch;
                }
            }
        }
    }

    fn release_pending(&mut self, heap: &mut Heap) {
        match std::mem::replace(&mut self.phase, DetachedPhase::Dispatch) {
            DetachedPhase::Dispatch => {}
            DetachedPhase::Call(mut call) => release_pins(heap, &mut call.pins),
            DetachedPhase::Require(require) => {
                crate::builtins::release_suspended_require(heap, require);
            }
            DetachedPhase::WaitForModule(_) => {}
            DetachedPhase::WaitForCoroutine(_) => {}
            DetachedPhase::DispatchWait(_) => {}
        }
    }
}

impl DetachedCall {
    fn scope_thread(&self, main_thread: RawGc<marker::Thread>) -> RawGc<marker::Thread> {
        match &self.target {
            SuspendedTarget::Active => main_thread,
            SuspendedTarget::Coroutine { thread, .. } => *thread,
        }
    }
}

impl From<SuspendedCall> for DetachedCall {
    fn from(call: SuspendedCall) -> Self {
        Self {
            future: call.future,
            host_requests: call.host_requests,
            pins: call.pins,
            site: AwaitSite {
                result_reg: call.result_reg,
                result_count: call.result_count,
                call_pc: call.call_pc,
                cleanup_end: call.cleanup_end,
            },
            target: call.target,
        }
    }
}

enum DetachedSourceReady {
    Id(crate::SourceResult<crate::ModuleId>),
    Source(crate::SourceResult<Vec<u8>>),
}

fn resume_detached_require(
    heap: &mut Heap,
    require: SuspendedRequire,
    ready: DetachedSourceReady,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    let SuspendedRequire {
        stage,
        result_reg,
        result_count,
        call_pc,
        cleanup_end,
        target,
    } = require;
    let site = AwaitSite {
        result_reg,
        result_count,
        call_pc,
        cleanup_end,
    };
    match (stage, ready) {
        (
            SuspendedRequireStage::Resolve {
                source, requester, ..
            },
            DetachedSourceReady::Id(Ok(id)),
        ) => resume_resolved_require(heap, &target, &source, id, &requester, site, root),
        (SuspendedRequireStage::Resolve { .. }, DetachedSourceReady::Id(Err(error))) => {
            resume_require_error(
                heap,
                &target,
                site,
                root,
                crate::builtins::require_resolve_error(&error),
            )
        }
        (
            SuspendedRequireStage::Read {
                id,
                instance,
                epoch,
                loading_key,
                ..
            },
            DetachedSourceReady::Source(Ok(source)),
        ) => resume_read_require(
            heap,
            &target,
            RequireReadReady {
                id,
                instance,
                epoch,
                loading_key,
                source,
            },
            site,
            root,
        ),
        (
            SuspendedRequireStage::Read {
                id, loading_key, ..
            },
            DetachedSourceReady::Source(Err(error)),
        ) => {
            let error = crate::builtins::finish_require_read_error(heap, &id, &loading_key, &error);
            resume_require_error(heap, &target, site, root, error)
        }
        _ => Err(DriverError::Poison),
    }
}

fn detached_phase(suspend: DriverSuspend) -> DetachedPhase {
    match suspend {
        DriverSuspend::Call(call) => DetachedPhase::Call(DetachedCall::from(call)),
        DriverSuspend::Require(require) => DetachedPhase::Require(require),
    }
}

fn allocate_detached_thread(
    heap: &mut Heap,
    globals: RawGc<marker::Table>,
) -> Result<(RawGc<marker::Thread>, RegistryRef), RuntimeError> {
    let mut thread = Thread::new();
    thread.globals = Some(globals);
    let handle = heap
        .alloc_thread(thread)
        .ok_or_else(|| RuntimeError::memory("out of memory creating a detached invocation"))?;
    if let Some(thread) = heap.thread_mut(handle) {
        thread.id = Some(handle);
    }
    let root = heap
        .pin(RawValue::Thread(handle))
        .ok_or_else(|| RuntimeError::memory("out of memory rooting a detached invocation"))?;
    Ok((handle, root))
}

fn detached_start_error(error: DriverError) -> RuntimeError {
    match error {
        DriverError::Runtime(failure) => {
            RuntimeError::with_kind("failed to create a detached invocation", failure.error.kind)
        }
        DriverError::Poison => RuntimeError::poisoned(),
    }
}

fn finish_detached_host_call(
    heap: &mut Heap,
    callback_thread: RawGc<marker::Thread>,
    outcome: DriverExec<Vec<RawValue>>,
) -> HostProtectedCallResult {
    match protect_driver_outcome(heap, outcome) {
        Ok(Ok(values)) => {
            owned_values_from_raw(heap, &values).map(|values| Ok(HostReturn { values }))
        }
        Ok(Err(failure)) => {
            let capture = heap
                .thread_mut(callback_thread)
                .and_then(|thread| thread.captured_traceback.take());
            owned_script_error_from_failure(heap, failure, capture).map(Err)
        }
        Err(error) => Err(error_from_host_protected_unwind(&error)),
    }
}

/// Completes when `deadline` passes; never completes when there is no wall-clock
/// deadline, so its `select!` arm stays inert.
async fn deadline_elapsed(deadline: Option<Instant>) {
    match deadline {
        Some(instant) if Instant::now() >= instant => {}
        Some(instant) => tokio::time::sleep_until(tokio::time::Instant::from_std(instant)).await,
        None => std::future::pending().await,
    }
}

/// Completes when `cancel` is tripped; never completes without a token.
async fn cancellation(cancel: Option<&Cancel>) {
    match cancel {
        Some(cancel) => cancel.cancelled().await,
        None => std::future::pending().await,
    }
}

fn stop_for_cancel(cancel: Option<&Cancel>) -> StopReason {
    cancel
        .and_then(Cancel::stop_reason)
        .unwrap_or(StopReason::Cancelled)
}

fn stop_for_deadline(governance: &Governance) -> StopReason {
    if let Some(cancel) = &governance.cancel {
        if cancel.is_cancelled() {
            return cancel.stop_reason().unwrap_or(StopReason::Cancelled);
        }
        cancel.stop(StopReason::Deadline);
    }
    StopReason::Deadline
}

/// Rewinds the active frame's `savedpc` to the suspended call site so an async
/// failure is located at the awaited `CALL`, not the instruction past it.
fn locate_at_call(thread: &mut Thread, call_pc: usize) {
    if let Some(frame) = thread
        .call_stack
        .iter_mut()
        .rev()
        .find_map(|entry| entry.frame_mut())
    {
        frame.savedpc = call_pc;
    }
}

/// Materializes an async host return — owned, heap-free data — into rooted
/// `RawValue`s after the await. Bytes intern through the accounted heap. Only the
/// results the `CALL` will observe are materialized (multret takes all; a
/// fixed-arity call takes `C-1`), so an ignored tail value is never interned or
/// rejected. The driver still releases call-scoped registry leases after this
/// function returns.
fn materialize_return(heap: &mut Heap, ret: &HostReturn, result_count: u8) -> Exec<Vec<RawValue>> {
    let want = if result_count == 0 {
        ret.values.len()
    } else {
        usize::from(result_count) - 1
    };
    let result = (|| {
        prepare_result_copy(heap, want, "host-return")?;
        ret.values
            .iter()
            .take(want)
            .map(|value| materialize_owned(heap, value))
            .collect()
    })();
    release_owned_pins(heap, &ret.values);
    result
}
