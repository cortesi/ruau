//! Synchronous coroutines (`coroutine.*`): create, resume, yield, status,
//! running.
//!
//! A coroutine is a heap [`Thread`] with its own registers and call frames. The
//! resume driver `std::mem::take`s the coroutine out of the arena, runs its
//! [`dispatch`] to the next yield or return — its registers disjoint from the
//! heap objects the run reads — then puts it back, so only the running thread's
//! state is borrowed mutably at a time. Yield is intercepted in
//! `precall`: it suspends with [`Step::Yield`], preserving the call stack for the
//! next resume; this driver turns that into the `resume` results.

use ruau_bytecode::{Instruction, opcodes::Opcode};

use crate::{
    api::{RawGc, RawValue, marker},
    call::{
        Exec, PrecallStep, capture_varargs_from_slice, catch_protected_error, closure_proto,
        collect_stack_results, complete_protected_results, empty_varargs, ensure_result_values,
        err, err_memory, err_value, materialize, place_results, precall, push_call_entry,
    },
    execute::{DispatchMode, dispatch},
    heap::Heap,
    scope::HostEntry,
    state::{
        CallInfo, CallStackEntry, CoroutineStatus, ResumeSlot, Step, SuspendedCall,
        SuspendedRequire, SuspendedTarget, Thread,
    },
};

/// `coroutine.create(f)`: a new suspended coroutine over the function `f`.
pub fn create(heap: &mut Heap, resumer: &Thread, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Function(entry) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err(
            "bad argument #1 to 'coroutine.create' (function expected)",
        ));
    };
    let mut co = Thread::new();
    co.status = CoroutineStatus::Suspended;
    co.entry = Some(entry);
    co.last_async_invocation = heap.current_async_invocation();
    // The coroutine shares the VM-wide environment with its creator; call-depth
    // enforcement reads the active VM limits when frames are pushed.
    co.globals = resumer.globals;
    let handle = heap
        .alloc_thread(co)
        .ok_or_else(|| err_memory("out of memory creating a coroutine"))?;
    // The coroutine anchors its own open upvalues at its own handle.
    if let Some(co) = heap.thread_mut(handle) {
        co.id = Some(handle);
    }
    Ok(vec![RawValue::Thread(handle)])
}

/// `coroutine.resume(co, ...)`: runs `co` until it yields or returns. Returns
/// `true` and the yielded/returned values, or `false` and the error value.
pub fn resume(
    heap: &mut Heap,
    resumer: &mut Thread,
    args: &[RawValue],
    host_entry: HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    let Some((co_value, resume_args)) = args.split_first() else {
        return Err(err(
            "bad argument #1 to 'coroutine.resume' (coroutine expected)",
        ));
    };
    let RawValue::Thread(handle) = *co_value else {
        return Err(err(
            "bad argument #1 to 'coroutine.resume' (coroutine expected)",
        ));
    };
    match resume_inner(
        heap,
        resumer,
        handle,
        &ResumeAction::Values(resume_args),
        false,
        host_entry,
    )? {
        CoroutineStep::Values(values) => Ok(values),
        CoroutineStep::Preempt => Err(err("unexpected preemption in coroutine.resume")),
        CoroutineStep::Suspend(call) => {
            release_suspended_pins(heap, call);
            Err(err(
                "attempt to await an async host call across a C-call boundary",
            ))
        }
        CoroutineStep::SuspendRequire(require) => {
            crate::builtins::release_suspended_require(heap, require);
            Err(err(
                "attempt to await an async require across a C-call boundary",
            ))
        }
        CoroutineStep::WaitForModule(_) => Err(err("required module is already loading")),
    }
}

/// Async-driver path for bytecode `coroutine.resume`: ordinary completion places
/// the `resume` results immediately, while a pending host call bubbles to the
/// root driver with enough metadata to resume the coroutine after the await.
pub fn resume_precal(
    heap: &mut Heap,
    resumer: &mut Thread,
    result_base: u32,
    result_count: u8,
    args: &[RawValue],
    preemptible: bool,
    host_entry: HostEntry<'_>,
) -> Exec<PrecallStep> {
    let Some((co_value, resume_args)) = args.split_first() else {
        return Err(err(
            "bad argument #1 to 'coroutine.resume' (coroutine expected)",
        ));
    };
    let RawValue::Thread(handle) = *co_value else {
        return Err(err(
            "bad argument #1 to 'coroutine.resume' (coroutine expected)",
        ));
    };
    match resume_inner(
        heap,
        resumer,
        handle,
        &ResumeAction::Values(resume_args),
        preemptible,
        host_entry,
    )? {
        CoroutineStep::Values(values) => {
            place_results(heap, resumer, result_base, result_count, &values)?;
            Ok(PrecallStep::Done)
        }
        CoroutineStep::Preempt => Ok(PrecallStep::Preempt),
        CoroutineStep::Suspend(mut call) => {
            call.target = SuspendedTarget::Coroutine {
                thread: handle,
                resume_result_reg: result_base,
                resume_result_count: result_count,
                resume_call_pc: 0,
            };
            Ok(PrecallStep::Suspend(call))
        }
        CoroutineStep::SuspendRequire(mut require) => {
            require.target = SuspendedTarget::Coroutine {
                thread: handle,
                resume_result_reg: result_base,
                resume_result_count: result_count,
                resume_call_pc: 0,
            };
            Ok(PrecallStep::SuspendRequire(require))
        }
        CoroutineStep::WaitForModule(loading_key) => Ok(PrecallStep::WaitForInFlight(loading_key)),
    }
}

/// Harness-only `resumeerror(co, error)`: resumes a suspended coroutine by
/// injecting `error` at its suspension point. This mirrors the upstream
/// conformance helper used to test protected-call unwinding across resume.
pub fn resume_error(
    heap: &mut Heap,
    resumer: &mut Thread,
    args: &[RawValue],
    host_entry: HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    let Some((co_value, error_args)) = args.split_first() else {
        return Err(err("bad argument #1 to 'resumeerror' (coroutine expected)"));
    };
    let RawValue::Thread(handle) = *co_value else {
        return Err(err("bad argument #1 to 'resumeerror' (coroutine expected)"));
    };
    let error = error_args.first().copied().unwrap_or(RawValue::Nil);
    match resume_inner(
        heap,
        resumer,
        handle,
        &ResumeAction::Error(error),
        false,
        host_entry,
    )? {
        CoroutineStep::Values(values) => Ok(values),
        CoroutineStep::Preempt => Err(err("unexpected preemption in coroutine.resume")),
        CoroutineStep::Suspend(call) => {
            release_suspended_pins(heap, call);
            Err(err(
                "attempt to await an async host call across a C-call boundary",
            ))
        }
        CoroutineStep::SuspendRequire(require) => {
            crate::builtins::release_suspended_require(heap, require);
            Err(err(
                "attempt to await an async require across a C-call boundary",
            ))
        }
        CoroutineStep::WaitForModule(_) => Err(err("required module is already loading")),
    }
}

enum ResumeAction<'a> {
    Values(&'a [RawValue]),
    Error(RawValue),
}

pub enum CoroutineStep {
    Values(Vec<RawValue>),
    Preempt,
    Suspend(SuspendedCall),
    SuspendRequire(SuspendedRequire),
    WaitForModule(crate::heap::ModuleCacheKey),
}

fn resume_inner(
    heap: &mut Heap,
    resumer: &mut Thread,
    handle: RawGc<marker::Thread>,
    action: &ResumeAction<'_>,
    preemptible: bool,
    host_entry: HostEntry<'_>,
) -> Exec<CoroutineStep> {
    let resumer_id = resumer
        .id
        .ok_or_else(|| err("resumer has no heap identity"))?;
    // Take the coroutine out of the arena so its registers are disjoint from the
    // heap objects its run reads; it is put back before returning.
    // A running or normal coroutine has been taken out elsewhere on the resumer
    // chain (its slot is a placeholder); a dead one is resident but finished.
    // Detect a non-resumable coroutine *without* taking it out — resuming it,
    // including resuming the running coroutine itself, is a `false, <message>`
    // result, not an error. Only a resident *suspended* coroutine is taken out.
    let resumable = heap
        .thread(handle)
        .is_some_and(|t| t.id == Some(handle) && t.status == CoroutineStatus::Suspended);
    if !resumable {
        let status = heap
            .thread(handle)
            .map_or(CoroutineStatus::Dead, |t| t.status);
        let value = materialize(
            heap,
            err(format!("cannot resume {} coroutine", status_name(status))),
        );
        return Ok(CoroutineStep::Values(vec![RawValue::Boolean(false), value]));
    }
    let mut co = heap
        .take_thread(handle)
        .ok_or_else(|| err("coroutine is not resident"))?;
    co.last_async_invocation = heap.current_async_invocation();
    co.status = CoroutineStatus::Running;
    // Stamp the placeholder left in the slot so a re-entrant `coroutine.status` or
    // `coroutine.resume` of this same coroutine sees it running, not the default.
    if let Some(slot) = heap.thread_mut(handle) {
        slot.status = CoroutineStatus::Running;
    }
    // The resumer is suspended while the coroutine runs; restore its prior status
    // (a nested resumer is itself `Normal`, the main thread `Running`) afterward.
    let resumer_status = resumer.status;
    resumer.status = CoroutineStatus::Normal;
    // Each resume runs a nested `dispatch` on the Rust stack. Inherit the resumer's
    // re-entry depth so a chain of coroutines that each resume the next shares one
    // bound (upstream `lua_resume` carries `from->nCcalls`); a per-thread counter
    // reset to 0 would let the chain overflow the host stack unmetered.
    resumer.native_depth += 1;
    co.native_depth = resumer.native_depth;
    // The body runs at this depth; a yield is allowed only here (see
    // `is_yieldable`), so record it as the coroutine's yieldable base.
    co.base_native_depth = co.native_depth;
    // While the child coroutine runs, its resumer is suspended but still owns
    // registers that open upvalues may reference. Put the resumer back in its
    // arena slot so ordinary open-upvalue lookup reaches it; take it out again
    // before returning to the suspended dispatch frame. Record the resumer on `co`
    // so a collection during the body roots the parked resumer chain (cleared below,
    // so a *suspended* coroutine carries no stale link).
    co.resumer = Some(resumer_id);
    park_thread(heap, resumer_id, resumer);
    let result = match action {
        ResumeAction::Values(resume_args) => {
            run_body(heap, &mut co, resume_args, preemptible, host_entry)
        }
        ResumeAction::Error(error) => {
            run_body_with_error(heap, &mut co, *error, preemptible, host_entry)
        }
    };
    unpark_thread(heap, resumer_id, resumer);
    co.resumer = None;
    resumer.native_depth -= 1;
    resumer.status = resumer_status;

    assert!(heap.put_thread(handle, co), "the slot is reserved");
    // An ordinary failure is the normal `false, <error>` resume result; a fatal
    // error (cancellation/deadline) raised inside the body propagates past `resume`
    // (and so past `coroutine.wrap`, which calls it), uncatchable. The coroutine is
    // already finalized and put back either way.
    result
}

fn park_thread(heap: &mut Heap, handle: RawGc<marker::Thread>, thread: &mut Thread) {
    let parked = std::mem::take(thread);
    assert!(
        heap.put_thread(handle, parked),
        "the thread slot is reserved while it is running"
    );
}

fn unpark_thread(heap: &mut Heap, handle: RawGc<marker::Thread>, thread: &mut Thread) {
    *thread = heap
        .take_thread(handle)
        .expect("the thread slot is reserved while it is running");
}

/// Runs the resumed coroutine's body to its next yield, return, or error, updating
/// its status. A *non-start* failure (too deep to begin, or a setup error) leaves
/// it `Suspended` and resumable; only an error raised *inside* the body kills it,
/// matching upstream's resume-error split.
fn run_body(
    heap: &mut Heap,
    co: &mut Thread,
    resume_args: &[RawValue],
    preemptible: bool,
    host_entry: HostEntry<'_>,
) -> Exec<CoroutineStep> {
    if co.native_depth > heap.limits().max_native_depth {
        co.status = CoroutineStatus::Suspended;
        return Ok(CoroutineStep::Values(vec![
            RawValue::Boolean(false),
            materialize(heap, err("stack overflow")),
        ]));
    }
    // The first resume builds the entry frame with the resume args as parameters;
    // a later resume makes them the values `yield` returns.
    let prepared = if co.call_stack.is_empty() {
        if let Some(slot) = co.resume_slot.take() {
            match slot {
                // A builtin/host entry (e.g. `coroutine.yield`) has no Lua frame,
                // so a yield from it leaves the call stack empty. Resuming
                // completes it: the builtin returns the resume values, and the
                // entry is finished.
                ResumeSlot::Direct { .. } => {
                    co.status = CoroutineStatus::Dead;
                    let results = result_values_from_slice(heap, resume_args, "coroutine")?;
                    return with_ok_step(heap, results);
                }
                ResumeSlot::ConformanceNative {
                    result_base,
                    result_count,
                    continuation,
                } => {
                    return continue_conformance_native(
                        heap,
                        co,
                        result_base,
                        result_count,
                        &continuation,
                        true,
                        preemptible,
                        host_entry,
                    );
                }
                ResumeSlot::Protected {
                    result_base,
                    result_count,
                } => complete_protected_results(
                    heap,
                    co,
                    result_base,
                    result_count,
                    true,
                    resume_args,
                ),
            }?;
        }
        let entry = co.entry.ok_or_else(|| err("coroutine has no function"))?;
        let proto = closure_proto(heap, entry)?;
        if heap
            .proto(proto)
            .is_some_and(|p| p.native.is_some() || p.host.is_some())
        {
            // First resume of a builtin/host entry: it has no bytecode frame, so
            // drive it through the yield-capable `precall` path — a synthetic
            // `CALL` of the entry at register 0 — rather than dispatching a frame.
            let nargs = resume_args.len();
            co.stacks
                .ensure(u32::try_from(nargs + 1).unwrap_or(u32::MAX))
                .map_err(|_| err_memory("not enough memory for the coroutine stack"))?;
            co.stacks.set(0, RawValue::Function(entry));
            for (index, &arg) in resume_args.iter().enumerate() {
                co.stacks
                    .set(u32::try_from(index + 1).unwrap_or(u32::MAX), arg);
            }
            co.top = u32::try_from(nargs + 1).unwrap_or(u32::MAX);
            let call = Instruction::abc(Opcode::Call, 0, u8::try_from(nargs + 1).unwrap_or(0), 0);
            match precall(heap, co, 0, &call, preemptible, host_entry) {
                Ok(PrecallStep::Yield(values)) => {
                    co.status = CoroutineStatus::Suspended;
                    return with_ok_step(heap, values);
                }
                Ok(PrecallStep::WaitForInFlight(loading_key)) => {
                    co.status = CoroutineStatus::Suspended;
                    return if heap.module_load_owned_by_current(&loading_key) {
                        with_ok_step(heap, Vec::new())
                    } else {
                        Ok(CoroutineStep::WaitForModule(loading_key))
                    };
                }
                Ok(PrecallStep::Preempt) => {
                    co.status = CoroutineStatus::Suspended;
                    return Ok(CoroutineStep::Preempt);
                }
                Ok(PrecallStep::Done) if co.call_stack.is_empty() => {
                    co.status = CoroutineStatus::Dead;
                    let results = collect_stack_results(heap, co, 0, co.top, "coroutine")?;
                    return with_ok_step(heap, results);
                }
                // A protected builtin entry (`pcall`/`xpcall`) pushed a frame; run it.
                Ok(PrecallStep::Done) => Ok(()),
                Ok(PrecallStep::Suspend(call)) => {
                    co.status = CoroutineStatus::Running;
                    return Ok(CoroutineStep::Suspend(call));
                }
                Ok(PrecallStep::SuspendRequire(require)) => {
                    co.status = CoroutineStatus::Running;
                    return Ok(CoroutineStep::SuspendRequire(require));
                }
                Err(error) if !error.is_catchable() => {
                    finalize_failed(heap, co);
                    return Err(error);
                }
                Err(error) => {
                    finalize_failed(heap, co);
                    return Ok(CoroutineStep::Values(died_with(
                        co,
                        materialize(heap, error),
                    )));
                }
            }
        } else {
            setup_entry(heap, co, resume_args)
        }
    } else {
        if let Some(slot) = co.resume_slot.take() {
            match slot {
                ResumeSlot::Direct {
                    result_base,
                    result_count,
                } => place_results(heap, co, result_base, result_count, resume_args),
                ResumeSlot::Protected {
                    result_base,
                    result_count,
                } => complete_protected_results(
                    heap,
                    co,
                    result_base,
                    result_count,
                    true,
                    resume_args,
                ),
                ResumeSlot::ConformanceNative {
                    result_base,
                    result_count,
                    continuation,
                } => {
                    return continue_conformance_native(
                        heap,
                        co,
                        result_base,
                        result_count,
                        &continuation,
                        false,
                        preemptible,
                        host_entry,
                    );
                }
            }
        } else {
            Ok(())
        }
    };
    if let Err(error) = prepared {
        // A non-start setup failure leaves the coroutine resumable; a fatal one
        // (cancellation/deadline) propagates past the resumer instead.
        if !error.is_catchable() {
            return Err(error);
        }
        co.status = CoroutineStatus::Suspended;
        return Ok(CoroutineStep::Values(vec![
            RawValue::Boolean(false),
            materialize(heap, error),
        ]));
    }
    continue_body_step(heap, co, preemptible, host_entry)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the conformance continuation boundary keeps the explicit host entry separate"
)]
fn continue_conformance_native(
    heap: &mut Heap,
    co: &mut Thread,
    result_base: u32,
    result_count: u8,
    continuation: &crate::state::ConformanceNativeContinuation,
    entry_continuation: bool,
    preemptible: bool,
    host_entry: HostEntry<'_>,
) -> Exec<CoroutineStep> {
    match crate::call::resume_conformance_native_continuation(
        co,
        result_base,
        result_count,
        continuation,
    )? {
        crate::call::ConformanceNativeStep::Yield(values) => {
            co.status = CoroutineStatus::Suspended;
            with_ok_step(heap, values)
        }
        crate::call::ConformanceNativeStep::Return(values) if entry_continuation => {
            co.status = CoroutineStatus::Dead;
            with_ok_step(heap, values)
        }
        crate::call::ConformanceNativeStep::Return(values) => {
            place_results(heap, co, result_base, result_count, &values)?;
            continue_body_step(heap, co, preemptible, host_entry)
        }
    }
}

/// Settles a coroutine body failure: a fatal (uncatchable) error finalizes
/// the coroutine and propagates so a tenant cannot catch a termination
/// signal through `coroutine.resume`/`wrap`; a catchable error kills the
/// coroutine and surfaces as the resume's `(false, error)` values.
fn settle_body_failure(
    heap: &mut Heap,
    co: &mut Thread,
    error: crate::call::RaisedError,
) -> Exec<CoroutineStep> {
    if !error.is_catchable() {
        finalize_failed(heap, co);
        return Err(error);
    }
    let error = crate::debug::locate(heap, co, error);
    finalize_failed(heap, co);
    Ok(CoroutineStep::Values(died_with(
        co,
        materialize(heap, error),
    )))
}

fn run_body_with_error(
    heap: &mut Heap,
    co: &mut Thread,
    error: RawValue,
    preemptible: bool,
    host_entry: HostEntry<'_>,
) -> Exec<CoroutineStep> {
    if co.native_depth > heap.limits().max_native_depth {
        co.status = CoroutineStatus::Suspended;
        return Ok(CoroutineStep::Values(vec![
            RawValue::Boolean(false),
            materialize(heap, err("stack overflow")),
        ]));
    }
    co.resume_slot = None;
    match catch_protected_error(heap, co, 0, err_value(error), host_entry) {
        Ok(()) => continue_body_step(heap, co, preemptible, host_entry),
        Err(error) => settle_body_failure(heap, co, error),
    }
}

pub fn continue_body_step(
    heap: &mut Heap,
    co: &mut Thread,
    preemptible: bool,
    host_entry: HostEntry<'_>,
) -> Exec<CoroutineStep> {
    // Active collection in a coroutine body is sound only when its resumer chain is
    // parked arena-resident and GC-rooted. Bytecode `coroutine.resume` sets
    // `co.resumer` and parks the resumer before the body runs; under the async root
    // driver that body may also propagate `Step::Preempt` to yield the lane. The
    // post-await async-driver resume (`resume_after_async_*`) has no `co.resumer`
    // because suspend cleared the chain, so run it as non-collecting native re-entry;
    // collection defers to the next rooted count-one context and the memory cap still
    // backstops.
    let mode = if co.resumer.is_some() {
        if preemptible {
            DispatchMode::CoroutinePreemptible
        } else {
            DispatchMode::Coroutine
        }
    } else {
        DispatchMode::NativeReentry
    };
    match dispatch(heap, co, 0, mode, host_entry) {
        Ok(Step::Return(values)) => {
            co.status = CoroutineStatus::Dead;
            with_ok_step(heap, values)
        }
        Ok(Step::Yield(values)) => {
            co.status = CoroutineStatus::Suspended;
            with_ok_step(heap, values)
        }
        Ok(Step::Preempt) => {
            co.status = CoroutineStatus::Suspended;
            Ok(CoroutineStep::Preempt)
        }
        Ok(Step::Suspend(call)) => {
            co.status = CoroutineStatus::Running;
            Ok(CoroutineStep::Suspend(call))
        }
        Ok(Step::SuspendRequire(require)) => {
            co.status = CoroutineStatus::Running;
            Ok(CoroutineStep::SuspendRequire(require))
        }
        Ok(Step::WaitForModule(loading_key)) => {
            co.status = CoroutineStatus::Suspended;
            if heap.module_load_owned_by_current(&loading_key) {
                with_ok_step(heap, Vec::new())
            } else {
                Ok(CoroutineStep::WaitForModule(loading_key))
            }
        }
        // A fatal error (cancellation/deadline) raised inside the coroutine is not
        // swallowed into a resume failure: finalize the coroutine and propagate it
        // past the resumer, so a tenant cannot use `coroutine.resume`/`wrap` to
        // catch a termination signal.
        Err(error) => settle_body_failure(heap, co, error),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the async resume boundary keeps the explicit host entry separate"
)]
pub fn resume_after_async_success(
    heap: &mut Heap,
    co: &mut Thread,
    call_pc: usize,
    result_reg: u32,
    result_count: u8,
    cleanup_end: u32,
    values: &[RawValue],
    host_entry: HostEntry<'_>,
) -> Exec<CoroutineStep> {
    match place_results(heap, co, result_reg, result_count, values) {
        Ok(()) => {
            crate::call::clear_call_temps(co, result_reg, values.len(), cleanup_end);
            continue_body_step(heap, co, false, host_entry)
        }
        Err(error) => resume_after_async_error(heap, co, call_pc, error, host_entry),
    }
}

pub fn resume_after_async_error(
    heap: &mut Heap,
    co: &mut Thread,
    call_pc: usize,
    error: crate::call::RaisedError,
    host_entry: HostEntry<'_>,
) -> Exec<CoroutineStep> {
    locate_at_call(co, call_pc);
    match catch_protected_error(heap, co, 0, error, host_entry) {
        Ok(()) => continue_body_step(heap, co, false, host_entry),
        Err(error) => settle_body_failure(heap, co, error),
    }
}

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

/// Terminates a failed coroutine while retaining the abandoned call stack's
/// debug metadata for post-mortem `debug.traceback(co)` / `debug.info(co, ...)`.
fn finalize_failed(heap: &mut Heap, co: &mut Thread) {
    let top = co
        .call_stack
        .iter()
        .rposition(|entry| entry.frame().is_some())
        .unwrap_or(0);
    let error_frames = co
        .call_stack
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.frame().map(|frame| (index, frame)))
        .map(|(index, frame)| crate::state::FrameSnapshot {
            closure: frame.closure,
            savedpc: if index == top {
                // The top abandoned frame should report the trapping
                // instruction's line; caller frames keep their return-site PC.
                frame.savedpc.saturating_add(1)
            } else {
                frame.savedpc
            },
        })
        .collect();
    finalize_dead(heap, co);
    co.error_frames = error_frames;
}

/// Terminates a coroutine that did not run to a normal `RETURN`: closes every
/// open upvalue over its abandoned registers and drops its frames, then marks it
/// dead. Without this an error or unsupported suspension leaves frames live and
/// upvalues open — a closure could read or mutate a dead coroutine's stack, and
/// the open upvalues (which pin `RawGc<Thread>`) would keep the whole `Thread`
/// alive.
pub fn finalize_dead(heap: &mut Heap, co: &mut Thread) {
    crate::execute::close_upvals_from(heap, co, 0);
    for entry in co.call_stack.drain(..) {
        if let CallStackEntry::Require(require) = entry {
            heap.module_load_end(&require.loading_key);
            heap.unpin(&require.module_pin);
        }
    }
    co.error_frames.clear();
    co.top = 0;
    co.status = CoroutineStatus::Dead;
}

/// `coroutine.status(co)`: the coroutine's lifecycle-state name.
pub fn status(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Thread(handle) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err(
            "bad argument #1 to 'coroutine.status' (coroutine expected)",
        ));
    };
    let status = heap
        .thread(handle)
        .map_or(CoroutineStatus::Dead, |t| t.status);
    let name = heap
        .intern_str(status_name(status).as_bytes())
        .ok_or_else(|| err_memory("out of memory interning a status"))?;
    Ok(vec![RawValue::String(name)])
}

/// `coroutine.close(co)`: forces a suspended or dead coroutine into the dead
/// state — its frames are dropped and its open upvalues closed (`finalize_dead`).
/// Errors on a running or normal coroutine, which cannot be closed from outside.
/// Returns `true`; `<close>`-local failure handling is not implemented. Reuses
/// the take-out model so the coroutine's registers stay disjoint from the heap
/// objects `close_upvals_from` touches.
pub fn close(heap: &mut Heap, thread: &Thread, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Thread(handle) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err(
            "bad argument #1 to 'coroutine.close' (coroutine expected)",
        ));
    };
    // The running coroutine is taken out of the arena for its own dispatch, so a
    // `take_thread` below would report it as merely "not resident". Detect it
    // first and give the real reason (upstream errors "cannot close a running
    // coroutine"), so closing `coroutine.running()` reports `running`.
    if thread.id == Some(handle) {
        return Err(err("cannot close a running coroutine"));
    }
    let mut co = match heap.take_thread(handle) {
        Some(co) => co,
        // A coroutine taken out elsewhere on the resume chain (a "normal"
        // ancestor that resumed another) is not resident but keeps its reserved
        // arena slot; a truly absent handle has none.
        None if heap.thread(handle).is_some() => {
            return Err(err("cannot close a normal coroutine"));
        }
        None => return Err(err("coroutine is not resident")),
    };
    let result = match co.status {
        CoroutineStatus::Suspended | CoroutineStatus::Dead => {
            finalize_dead(heap, &mut co);
            // A coroutine that died from an error surfaces it once: the first
            // close returns `(false, error)`, a later close returns `(true)`.
            match co.death_error.take() {
                Some(error) => Ok(vec![RawValue::Boolean(false), error]),
                None => Ok(vec![RawValue::Boolean(true)]),
            }
        }
        running_or_normal => Err(err(format!(
            "cannot close a {} coroutine",
            status_name(running_or_normal)
        ))),
    };
    assert!(heap.put_thread(handle, co), "the slot is reserved");
    result
}

/// `coroutine.running()`: the running coroutine, or `nil` on the main thread —
/// one value, as upstream `corunning` returns. The main thread has no entry
/// function.
pub fn running(thread: &Thread) -> Exec<Vec<RawValue>> {
    let value = if thread.entry.is_none() {
        RawValue::Nil
    } else {
        thread.id.map_or(RawValue::Nil, RawValue::Thread)
    };
    Ok(vec![value])
}

/// `coroutine.isyieldable()`: whether the running Lua frame is outside a
/// non-yieldable native re-entry. Top-level Lua and coroutine bodies are
/// yieldable; metamethod/`pcall`/builtin re-entry is not (`nCcalls <= baseCcalls`).
pub fn is_yieldable(thread: &Thread) -> Exec<Vec<RawValue>> {
    let yieldable = thread.native_depth == thread.base_native_depth;
    Ok(vec![RawValue::Boolean(yieldable)])
}

/// Prepends `true` to a coroutine's yielded/returned values.
fn with_ok(heap: &Heap, mut values: Vec<RawValue>) -> Exec<Vec<RawValue>> {
    ensure_result_values(heap, values.len(), "coroutine")?;
    values
        .try_reserve(1)
        .map_err(|_| err_memory("not enough memory for coroutine results"))?;
    values.insert(0, RawValue::Boolean(true));
    Ok(values)
}

fn with_ok_step(heap: &Heap, values: Vec<RawValue>) -> Exec<CoroutineStep> {
    with_ok(heap, values).map(CoroutineStep::Values)
}

fn release_suspended_pins(heap: &mut Heap, call: SuspendedCall) {
    for reference in call.pins {
        heap.unpin(&reference);
    }
}

fn result_values_from_slice(
    heap: &Heap,
    values: &[RawValue],
    context: &str,
) -> Exec<Vec<RawValue>> {
    ensure_result_values(heap, values.len(), context)?;
    let mut results = Vec::new();
    results
        .try_reserve(values.len())
        .map_err(|_| err_memory("not enough memory for result values"))?;
    results.extend_from_slice(values);
    Ok(results)
}

/// Records the error a coroutine just died from — so `coroutine.close` can
/// surface it once — and builds the `resume` failure result `(false, error)`.
fn died_with(co: &mut Thread, error: RawValue) -> Vec<RawValue> {
    co.death_error = Some(error);
    vec![RawValue::Boolean(false), error]
}

fn status_name(status: CoroutineStatus) -> &'static str {
    match status {
        CoroutineStatus::Suspended => "suspended",
        CoroutineStatus::Running => "running",
        CoroutineStatus::Normal => "normal",
        CoroutineStatus::Dead => "dead",
    }
}

/// Builds the coroutine's first frame: its entry function with the resume args as
/// its parameters.
fn setup_entry(heap: &Heap, co: &mut Thread, args: &[RawValue]) -> Exec<()> {
    let entry = co.entry.ok_or_else(|| err("coroutine has no function"))?;
    let proto = closure_proto(heap, entry)?;
    let (num_params, is_vararg, max_stack) = heap
        .proto(proto)
        .map(|p| {
            (
                u32::from(p.num_params),
                p.is_vararg,
                u32::from(p.max_stack_size).max(1),
            )
        })
        .ok_or_else(|| err("coroutine function has no prototype"))?;
    let nargs = u32::try_from(args.len()).unwrap_or(u32::MAX);
    // A variadic entry keeps the first-resume arguments past its fixed parameters
    // as metered side storage after its registers are reused.
    let varargs = if is_vararg && args.len() > num_params as usize {
        capture_varargs_from_slice(heap, args, num_params as usize)?
    } else {
        empty_varargs(heap)
    };
    // Reserve for the args too: a resume with more arguments than the entry's
    // stack window must still grow through the accounted `ensure`, not the
    // unmetered `set` resize path.
    co.stacks
        .ensure(max_stack.max(nargs))
        .map_err(|_| err_memory("not enough memory for the coroutine stack"))?;
    for (i, &arg) in args.iter().enumerate() {
        co.stacks.set(u32::try_from(i).unwrap_or(u32::MAX), arg);
    }
    for i in nargs..num_params {
        co.stacks.set(i, RawValue::Nil);
    }
    push_call_entry(
        heap,
        co,
        CallStackEntry::Frame(CallInfo {
            closure: entry,
            proto,
            base: 0,
            result_base: 0,
            frame_top: max_stack,
            savedpc: 0,
            nresults: -1,
            varargs,
        }),
    )?;
    co.top = max_stack;
    Ok(())
}
