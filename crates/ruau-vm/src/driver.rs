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

use std::{cell::RefCell, panic::AssertUnwindSafe, time::Instant};

use ruau_vm_api::{
    HostError, HostReturn, OwnedValue, RawGc, RawValue, RegistryRef, Unwind, marker,
};

use crate::{
    call::{
        Exec, ProtectedFailure, RaisedError, RuntimeErrorKind, catch_protected_error, err,
        err_cancelled, err_deadline, error_payload_from_message, materialize, materialize_owned,
        place_results, prepare_result_copy, push_call_entry, release_owned_pins, root_frame,
        root_function_frame, unwind_protected_failure,
    },
    cancel::Cancel,
    coroutine::{self, CoroutineStep},
    execute::{DispatchMode, dispatch},
    heap::Heap,
    host::{
        HostProtectedCallRequest, HostRequest, HostRequests, HostScopeRequest, HostScriptError,
        ProtectedArgsOperation,
    },
    scope::{AppData, RuntimeError, Scope},
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
    app_data: &RefCell<AppData>,
) -> Result<Vec<RawValue>, Unwind> {
    let outcome = protected_async_main(heap, main_thread, main, governance, app_data, None).await;
    unwind_driver_outcome(heap, outcome)
}

/// Runs `main` on the async driver in a protected mode: catchable script errors
/// become the inner `Err`, while fatal control-flow errors stay outer `Err`s.
pub async fn run_async_protected(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    main: RawGc<marker::Closure>,
    governance: &Governance,
    app_data: &RefCell<AppData>,
    max_traceback_bytes: usize,
) -> Result<Result<Vec<RawValue>, ProtectedFailure>, Unwind> {
    let outcome = protected_async_main(
        heap,
        main_thread,
        main,
        governance,
        app_data,
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
    app_data: &RefCell<AppData>,
    max_traceback_bytes: usize,
) -> Result<Result<Vec<RawValue>, ProtectedFailure>, Unwind> {
    let outcome = protected_async_function(
        heap,
        main_thread,
        func,
        args,
        governance,
        app_data,
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
    app_data: &RefCell<AppData>,
    f: impl FnOnce(&mut Heap, &mut Thread) -> DriverExec<T>,
) -> DriverExec<T> {
    heap.drain_releases();
    let Some(mut thread) = heap.take_thread(handle) else {
        return Err(DriverError::Poison);
    };
    let _host_app_data = heap.enter_host_app_data(app_data);
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
    app_data: &'a RefCell<AppData>,
    traceback_limit: Option<usize>,
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
}

fn start_protected_async(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    app_data: &RefCell<AppData>,
    build_frame: impl FnOnce(&mut Heap, &mut Thread) -> Exec<CallInfo>,
) -> DriverExec<ProtectedState> {
    with_thread_segment(heap, main_thread, app_data, |heap, thread| {
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
    app_data: &RefCell<AppData>,
    traceback_limit: Option<usize>,
) -> DriverExec<Vec<RawValue>> {
    let state = start_protected_async(heap, main_thread, app_data, |heap, thread| {
        root_frame(heap, thread, main)
    })?;
    drive_protected_async(
        heap,
        main_thread,
        governance,
        state,
        app_data,
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
    app_data: &RefCell<AppData>,
    traceback_limit: Option<usize>,
) -> DriverExec<Vec<RawValue>> {
    let state = start_protected_async(heap, main_thread, app_data, |heap, thread| {
        root_function_frame(heap, thread, func, &args)
    })?;
    drop(args);
    drive_protected_async(
        heap,
        main_thread,
        governance,
        state,
        app_data,
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
    app_data: &RefCell<AppData>,
    traceback_limit: Option<usize>,
) -> DriverExec<Vec<RawValue>> {
    loop {
        // Synchronous segment: run to the next step while borrowing the VM. This is
        // the driver's root-async dispatch, so it may return `Preempt`.
        let root_step = with_thread_segment(heap, main_thread, app_data, |heap, thread| {
            match dispatch(heap, thread, state.floor, DispatchMode::RootAsync) {
                Ok(Step::Return(results)) => Ok(RootStep::Return(results)),
                // A yield that reaches the driver's root crossed the main thread.
                Ok(Step::Yield(_)) => {
                    let error = err("attempt to yield across the main thread");
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
                // The cooperative quantum is spent: yield the worker so other VMs on the
                // runtime make progress, then re-enter at the preserved `savedpc`.
                Ok(Step::Preempt) => Ok(RootStep::Preempt),
                Ok(Step::Suspend(call)) => Ok(RootStep::Suspend(call)),
                Ok(Step::SuspendRequire(require)) => Ok(RootStep::SuspendRequire(require)),
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
        })?;
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
                        app_data,
                        traceback_limit,
                        err_deadline("deadline exceeded at a preemption checkpoint"),
                    ));
                }
                tokio::task::yield_now().await;
                continue;
            }
            RootStep::Suspend(call) => DriverSuspend::Call(call),
            RootStep::SuspendRequire(require) => DriverSuspend::Require(require),
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
                        app_data,
                        traceback_limit,
                    )
                    .await
                }
                DriverSuspend::Require(require) => {
                    let root = DriverRoot {
                        main_thread,
                        state,
                        app_data,
                        traceback_limit,
                    };
                    resume_awaited_require(heap, require, governance, root).await
                }
            };
            match resumed {
                Ok(DriverResume::Continue) => break,
                Ok(DriverResume::Suspend(next)) => suspended = *next,
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
}

impl DriverResume {
    fn suspend(next: DriverSuspend) -> Self {
        Self::Suspend(Box::new(next))
    }
}

async fn resume_awaited_call(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    call: SuspendedCall,
    governance: &Governance,
    state: ProtectedState,
    app_data: &RefCell<AppData>,
    traceback_limit: Option<usize>,
) -> DriverExec<DriverResume> {
    let SuspendedCall {
        future,
        host_requests,
        mut pins,
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
        app_data,
    )
    .await?;
    match target {
        SuspendedTarget::Active => {
            with_thread_segment(heap, main_thread, app_data, |heap, thread| {
                resume_active_await(heap, thread, pins, site, outcome, state, traceback_limit)
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
                    app_data,
                    traceback_limit,
                    err("suspended coroutine is not resident"),
                ));
            };
            let _host_app_data = heap.enter_host_app_data(app_data);
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                Ok(resume_coroutine_await(heap, &mut co, pins, site, outcome)?)
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
                        app_data,
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
                    app_data,
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
                CoroutineStep::Preempt => unreachable!(
                    "post-await coroutine resumes are non-preemptible until the resumer chain is rooted"
                ),
            }
        }
    }
}

enum SourceAwait<T> {
    Ready(crate::ModuleSourceResult<T>),
    Deadline,
    Cancelled,
}

async fn await_module_source<T>(
    mut future: crate::ModuleSourceFuture<T>,
    governance: &Governance,
) -> SourceAwait<T> {
    tokio::select! {
        biased;
        result = &mut future => SourceAwait::Ready(result),
        () = deadline_elapsed(governance.deadline) => SourceAwait::Deadline,
        () = cancellation(governance.cancel.as_ref()) => SourceAwait::Cancelled,
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
            SourceAwait::Deadline => resume_require_error(
                heap,
                &target,
                site,
                root,
                err_deadline("deadline exceeded while awaiting module source"),
            ),
            SourceAwait::Cancelled => {
                resume_require_error(heap, &target, site, root, err_cancelled())
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
            SourceAwait::Deadline => {
                crate::builtins::clear_require_loading(heap, &loading_key);
                resume_require_error(
                    heap,
                    &target,
                    site,
                    root,
                    err_deadline("deadline exceeded while awaiting module source"),
                )
            }
            SourceAwait::Cancelled => {
                crate::builtins::clear_require_loading(heap, &loading_key);
                resume_require_error(heap, &target, site, root, err_cancelled())
            }
        },
    }
}

fn resume_resolved_require(
    heap: &mut Heap,
    target: &SuspendedTarget,
    source: &std::sync::Arc<dyn crate::ModuleSource>,
    id: crate::ModuleId,
    requester: &Option<crate::ModuleId>,
    site: AwaitSite,
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    match target {
        SuspendedTarget::Active => {
            with_thread_segment(heap, root.main_thread, root.app_data, |heap, thread| {
                match crate::builtins::continue_require_after_resolve(
                    heap,
                    thread,
                    source,
                    id,
                    requester,
                    &crate::builtins::RequireCallSite {
                        result_reg: site.result_reg,
                        result_count: site.result_count,
                        cleanup_end: site.cleanup_end,
                    },
                ) {
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
                    root.app_data,
                    root.traceback_limit,
                    err("suspended coroutine is not resident"),
                ));
            };
            let _host_app_data = heap.enter_host_app_data(root.app_data);
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
            with_thread_segment(heap, root.main_thread, root.app_data, |heap, thread| {
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
                    root.app_data,
                    root.traceback_limit,
                    err("suspended coroutine is not resident"),
                ));
            };
            let _host_app_data = heap.enter_host_app_data(root.app_data);
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
        crate::builtins::RequireCallStep::WaitForInFlight => resume_active_require_error(
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
            with_thread_segment(heap, root.main_thread, root.app_data, |heap, thread| {
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
        crate::builtins::RequireCallStep::WaitForInFlight => {
            suspend_coroutine_require_wait(heap, coroutine_thread, resume_site, require_site, root)
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
    root: DriverRoot<'_>,
) -> DriverExec<DriverResume> {
    heap.drain_releases();
    let Some(mut co) = heap.take_thread(coroutine_thread) else {
        return Err(unwind_main(
            heap,
            root.main_thread,
            root.state,
            root.app_data,
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
    resume_coroutine_step(
        heap,
        resume_site,
        CoroutineStep::Values(vec![RawValue::Boolean(true)]),
        coroutine_thread,
        root,
    )
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
            root.app_data,
            root.traceback_limit,
            err("suspended coroutine is not resident"),
        ));
    };
    let _host_app_data = heap.enter_host_app_data(root.app_data);
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
                root.app_data,
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
        crate::coroutine::continue_body_step(heap, co, false)
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
        crate::coroutine::resume_after_async_error(heap, co, require_site.call_pc, error)
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
            root.app_data,
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
        CoroutineStep::Preempt => unreachable!(
            "post-await coroutine resumes are non-preemptible until the resumer chain is rooted"
        ),
    }
}

fn place_coroutine_resume_results(
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    state: ProtectedState,
    app_data: &RefCell<AppData>,
    traceback_limit: Option<usize>,
    site: ResumeSite,
    values: &[RawValue],
) -> DriverExec<DriverResume> {
    with_thread_segment(heap, main_thread, app_data, |heap, thread| {
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
    app_data: &RefCell<AppData>,
    traceback_limit: Option<usize>,
    error: RaisedError,
) -> DriverError {
    match with_thread_segment(heap, main_thread, app_data, |heap, thread| {
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

fn resume_active_await(
    heap: &mut Heap,
    thread: &mut Thread,
    mut pins: Vec<ruau_vm_api::RegistryRef>,
    site: AwaitSite,
    outcome: Result<HostReturn, AwaitFailure>,
    state: ProtectedState,
    traceback_limit: Option<usize>,
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
            );
        }
    };
    let materialized = materialize_return(heap, &host_return, site.result_count);
    release_pins(heap, &mut pins);
    let values = match materialized {
        Ok(values) => values,
        Err(error) => {
            locate_at_call(thread, site.call_pc);
            return resume_active_error(heap, thread, state, traceback_limit, error);
        }
    };
    if let Err(error) = place_results(heap, thread, site.result_reg, site.result_count, &values) {
        locate_at_call(thread, site.call_pc);
        return resume_active_error(heap, thread, state, traceback_limit, error);
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
) -> DriverExec<DriverResume> {
    match catch_protected_error(heap, thread, state.floor, error) {
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
    mut pins: Vec<ruau_vm_api::RegistryRef>,
    site: AwaitSite,
    outcome: Result<HostReturn, AwaitFailure>,
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
            );
        }
    };
    let materialized = materialize_return(heap, &host_return, site.result_count);
    release_pins(heap, &mut pins);
    let values = match materialized {
        Ok(values) => values,
        Err(error) => return coroutine::resume_after_async_error(heap, co, site.call_pc, error),
    };
    coroutine::resume_after_async_success(
        heap,
        co,
        site.call_pc,
        site.result_reg,
        site.result_count,
        site.cleanup_end,
        &values,
    )
}

fn release_pins(heap: &mut Heap, pins: &mut Vec<ruau_vm_api::RegistryRef>) {
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
    Deadline,
    /// The request was cancelled while the future was pending.
    Cancelled,
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
        AwaitFailure::Deadline => err_deadline("deadline exceeded while awaiting a host call"),
        AwaitFailure::Cancelled => err_cancelled(),
    }
}

/// Awaits a pending host future under the request's governance: it resolves to
/// the host's result, or to a deadline / cancellation failure if either trips
/// first. This is the production driver's `select!`; the deterministic
/// model drives `dispatch`/resume directly with scripted completions instead.
async fn await_governed(
    mut future: ruau_vm_api::HostFuture,
    mut host_requests: Option<HostRequests>,
    governance: &Governance,
    heap: &mut Heap,
    main_thread: RawGc<marker::Thread>,
    scope_thread: RawGc<marker::Thread>,
    app_data: &RefCell<AppData>,
) -> DriverExec<Result<HostReturn, AwaitFailure>> {
    loop {
        let Some(requests) = host_requests.as_mut() else {
            return Ok(tokio::select! {
                biased;
                result = &mut future => result.map_err(AwaitFailure::Host),
                () = deadline_elapsed(governance.deadline) => Err(AwaitFailure::Deadline),
                () = cancellation(governance.cancel.as_ref()) => Err(AwaitFailure::Cancelled),
            });
        };
        tokio::select! {
            // Bias toward the future: a future already ready when the deadline has
            // also passed still delivers its result rather than spuriously timing out.
            biased;
            result = &mut future => return Ok(result.map_err(AwaitFailure::Host)),
            () = deadline_elapsed(governance.deadline) => {
                return Ok(Err(AwaitFailure::Deadline));
            }
            () = cancellation(governance.cancel.as_ref()) => {
                return Ok(Err(AwaitFailure::Cancelled));
            }
            request = requests.recv() => {
                if let Some(request) = request {
                    service_host_request(
                        heap,
                        main_thread,
                        scope_thread,
                        governance,
                        app_data,
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
    app_data: &RefCell<AppData>,
    request: HostRequest,
) -> DriverExec<()> {
    match request {
        HostRequest::Scope(request) => service_scope_request(heap, scope_thread, app_data, request),
        HostRequest::ProtectedCall(request) => {
            service_protected_call_request(
                heap,
                main_thread,
                scope_thread,
                governance,
                app_data,
                request,
            )
            .await
        }
    }
}

fn service_scope_request(
    heap: &mut Heap,
    scope_thread: RawGc<marker::Thread>,
    app_data: &RefCell<AppData>,
    request: HostScopeRequest,
) -> DriverExec<()> {
    with_thread_segment(heap, scope_thread, app_data, |heap, thread| {
        let entered_scope = heap.try_enter_scope();
        let scope = Scope::with_scope_guard(heap, thread, app_data, entered_scope);
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
    app_data: &RefCell<AppData>,
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
        app_data,
        &callback,
        convert_args,
    )?;
    match prepared {
        Ok(prepared) => {
            let result = run_host_protected_call(heap, governance, app_data, prepared).await;
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
    app_data: &RefCell<AppData>,
    callback: &crate::scope::Stashed<marker::Closure>,
    convert_args: ProtectedArgsOperation,
) -> DriverExec<Result<PreparedHostProtectedCall, RuntimeError>> {
    with_thread_segment(heap, scope_thread, app_data, |heap, thread| {
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
        let entered_scope = heap.try_enter_scope();
        let scope = Scope::with_scope_guard(heap, thread, app_data, entered_scope);
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
    app_data: &RefCell<AppData>,
    mut prepared: PreparedHostProtectedCall,
) -> Result<Result<HostReturn, HostScriptError>, RuntimeError> {
    let outcome = Box::pin(run_async_function_protected(
        heap,
        prepared.callback_thread,
        prepared.callback,
        std::mem::take(&mut prepared.args),
        governance,
        app_data,
        crate::SCRIPT_ERROR_TRACEBACK_MAX_BYTES,
    ))
    .await;
    let converted = match outcome {
        Ok(Ok(values)) => {
            owned_values_from_raw(heap, &values).map(|values| Ok(HostReturn { values }))
        }
        Ok(Err(failure)) => owned_script_error_from_failure(heap, failure).map(Err),
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
) -> Result<HostScriptError, RuntimeError> {
    let kind = failure.error.kind;
    let traceback = failure.traceback;
    let raw = materialize(heap, failure.error);
    let value = owned_value_from_raw(heap, raw)?;
    Ok(HostScriptError::new(value, kind, traceback))
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
