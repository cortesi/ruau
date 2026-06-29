//! The synchronous dispatch loop (port `lvmexecute.cpp`).
//!
//! Registers live in the heap's `StackStore`; a `CallInfo` is a window into it.
//! Lua calls are iterative: `CALL` pushes a frame and the loop continues in the
//! callee, `RETURN` pops it, so a deep Lua chain never recurses in Rust. The loop
//! covers loads, arithmetic, comparisons and jumps, numeric `for`, varargs,
//! closures with full upvalue capture, call/return, raw table fast paths, and the
//! implemented metamethods. Unsupported opcodes raise a clean runtime error
//! rather than misbehaving.

use ruau_bytecode::{
    Instruction,
    opcodes::{
        CaptureType, FORGLOOP_VARS_MASK, IMPORT_PATH_COMPONENT_MASK, IMPORT_PATH_COUNT_SHIFT,
        JUMPX_K_INDEX_MASK, JUMPX_K_NOT_BIT, Opcode, import_component_shift,
    },
};
use ruau_vm_api::{RawGc, RawValue, marker};

use crate::{
    call::{
        Exec, PrecallStep, call_value, catch_protected_error, collect_stack_results, err,
        err_cancelled, err_deadline, err_gas, err_memory, err_memory_limit, err_register_stack_oom,
        has_protected_boundary, precall, prepare_result_copy, return_op,
    },
    func::{Closure, UpVal},
    heap::Heap,
    object::{Proto, RuntimeConstant, TableShape},
    state::{Step, Thread},
    table::LuaTable,
    tm::{self, MetaEvent},
    vmutils::{self, ArithOp},
};

/// The minimum stack the dispatch guard keeps free, and the segment it allocates
/// when growing. A nested metamethod, resume, or host-root call can run a fresh
/// `dispatch` frame, and those frames are large; without stack growth, the Rust
/// stack could overflow before the `native_depth` cap raises.
///
/// `stacker` degrades to a no-op on targets without `psm` stack-switching support
/// (wasm32, exotic architectures, and under Miri), where this protection is
/// absent. So stack protection never silently vanishes there,
/// `DEFAULT_MAX_NATIVE_DEPTH` drops to a conservative value on those targets
/// (see limits.rs); the supported native targets (x86_64/aarch64 on
/// Linux/macOS/Windows) have real stack growth.
const DISPATCH_RED_ZONE: usize = 256 * 1024;
const DISPATCH_STACK_SIZE: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub enum DispatchMode {
    /// Root synchronous VM entry: active GC is sound, but worker preemption is not used.
    RootSync,
    /// Root async VM entry: active GC and cooperative worker preemption are both enabled.
    RootAsync,
    /// A coroutine body. Its resumer (and every ancestor resumer) is parked
    /// arena-resident via `park_thread` and kept a GC root through the `Thread::resumer`
    /// chain, so exactly the running coroutine is taken out (count one) and active
    /// collection here is sound.
    Coroutine,
    /// A coroutine body running under the async root driver. It has the same active-GC
    /// contract as [`Coroutine`](Self::Coroutine), and also propagates cooperative
    /// preemption back to the root async driver.
    CoroutinePreemptible,
    /// Native re-entry — a metamethod or a host `Scope::call` running by native
    /// recursion on the *same* taken-out thread, with the *caller's* unrooted
    /// temporaries potentially live on the Rust stack (not in any traced register
    /// stack). Active collection is **not** legal here, and a yield is impossible.
    NativeReentry,
}

impl DispatchMode {
    fn may_collect_active(self) -> bool {
        matches!(
            self,
            Self::RootSync | Self::RootAsync | Self::Coroutine | Self::CoroutinePreemptible
        )
    }

    pub(crate) fn may_preempt(self) -> bool {
        matches!(self, Self::RootAsync | Self::CoroutinePreemptible)
    }
}

/// The dispatch loop. Runs the frames above `floor` until one of them returns the
/// call stack back down to `floor` ([`Step::Return`]) or `coroutine.yield`
/// suspends it ([`Step::Yield`]). A `CALL` pushes a frame and continues in the
/// callee; a `RETURN` pops one.
///
/// Each call (the outermost run and every native re-entry) is guarded by
/// [`stacker::maybe_grow`], so deep recursion through metamethods, resume, or
/// other native re-entry grows the native stack on demand and stays bounded by
/// the catchable `native_depth` cap instead of overflowing the OS stack.
pub fn dispatch(
    heap: &mut Heap,
    thread: &mut Thread,
    floor: usize,
    mode: DispatchMode,
) -> Exec<Step> {
    stacker::maybe_grow(DISPATCH_RED_ZONE, DISPATCH_STACK_SIZE, || {
        loop {
            let step = if heap.gas_profile_active() {
                dispatch_inner::<true>(heap, thread, floor, mode)
            } else {
                dispatch_inner::<false>(heap, thread, floor, mode)
            };
            match step {
                Ok(step) => return Ok(step),
                Err(error) => {
                    if has_protected_boundary(thread, floor) {
                        catch_protected_error(heap, thread, floor, error)?;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    })
}

/// Instructions between batched safepoints (cancellation, logical deadline,
/// preemption quantum): each fires within one interval, which is prompt at
/// any plausible quantum or deadline granularity.
///
/// GC checks deliberately stay per-instruction: collection timing is
/// semantically load-bearing under the generational heap — a collection that
/// lands where a weak-table value is still register-rooted promotes it past
/// minor-collection clearing (caught by closure.luau's collect-until-weak-
/// clear loop), so debt-paced collections must keep firing at the exact
/// allocation boundary that crossed the threshold, not at a batched boundary.
const SAFEPOINT_INTERVAL: u32 = 64;

/// The batched dispatch safepoint: cancellation, the logical deadline, and
/// cooperative preemption. `elapsed` is the number of instructions executed
/// since the previous safepoint, charged against the preemption quantum in
/// one batch. GC stays in the per-instruction path (see
/// [`SAFEPOINT_INTERVAL`] for why).
#[inline(never)]
fn batched_safepoint(heap: &mut Heap, mode: DispatchMode, elapsed: u32) -> Exec<Option<Step>> {
    // Cancellation is honoured here too, so a synchronous CPU loop with no host
    // await still stops promptly when the request is cancelled. It is fatal —
    // uncatchable by `pcall` — so a tenant cannot swallow it and keep running.
    if heap.is_cancelled() {
        return Err(err_cancelled());
    }
    // The logical deadline reads the gas-spent counter as its clock, so a
    // deterministic harness gets the same fatal deadline behavior a wall
    // deadline gives production requests.
    if heap.logical_deadline_exceeded() {
        return Err(err_deadline("logical deadline exceeded"));
    }
    // Cooperative preemption: the async driver's root dispatch and its
    // preemptible coroutine bodies yield, so long CPU-bound scripts periodically
    // release the worker. Synchronous root dispatch can collect but does not
    // preempt; true native re-entry does neither.
    if mode.may_preempt() && heap.consume_quantum(elapsed) {
        return Ok(Some(Step::Preempt));
    }
    Ok(None)
}

type ActiveInstruction = (
    usize,
    u32,
    RawGc<marker::Closure>,
    usize,
    RawGc<Proto>,
    Instruction,
);

#[inline]
fn active_instruction(heap: &Heap, thread: &Thread) -> Exec<ActiveInstruction> {
    // An empty stack means a `RETURN` unbalanced its `CALL` — only reachable
    // with crafted trusted bytecode, but return an error rather than panic.
    let active = thread
        .call_stack
        .len()
        .checked_sub(1)
        .ok_or_else(|| err("call stack underflow"))?;
    let frame = thread.call_stack[active]
        .frame()
        .ok_or_else(|| err("protected boundary reached as an executable frame"))?;
    let base = frame.base;
    let closure = frame.closure;
    let pc = frame.savedpc;
    // Resolved once at frame push: fetching the instruction costs one arena
    // access, not a closure deref plus a proto deref.
    let proto = frame.proto;
    let Some(instr) = heap.proto(proto).and_then(|p| p.instruction(pc)) else {
        return Err(err("program counter past end of code"));
    };
    Ok((active, base, closure, pc, proto, instr))
}

fn dispatch_inner<const PROFILE_GAS: bool>(
    heap: &mut Heap,
    thread: &mut Thread,
    floor: usize,
    mode: DispatchMode,
) -> Exec<Step> {
    // Instructions executed since the last batched safepoint; the safepoint
    // charges exactly this many units against the preemption quantum.
    let mut since_safepoint: u32 = 0;
    // Run one safepoint before the first instruction of every dispatch
    // segment, so a pre-cancelled request fails before executing anything
    // (it charges zero quantum).
    let mut entry_safepoint = true;
    loop {
        let profiled_instruction = if PROFILE_GAS {
            let instruction = active_instruction(heap, thread)?;
            heap.set_current_gas_site(instruction.4, instruction.3);
            // Spend one unit of the request's instruction budget after tagging
            // the current source site, so the profiled dispatch variant can
            // attribute the same gas the ordinary loop charges.
            if !heap.tick_gas_profiled() {
                return Err(err_gas());
            }
            Some(instruction)
        } else {
            // Spend one unit of the request's instruction budget; a depleted
            // budget halts execution so untrusted bytecode cannot spin a loop
            // unmetered. The non-profiled loop keeps the historical ordering
            // and avoids the profile branch inside the hot tick.
            if !heap.tick_gas_unprofiled() {
                return Err(err_gas());
            }
            None
        };
        // The memory safepoint: a script that has grown its heap past the per-VM cap is
        // stopped here with a catchable error, before the process backstop — but first
        // try to reclaim. Active collection is tied to the root dispatch mode, not to
        // async preemption: synchronous `Vm::call` can collect under the cap, while
        // nested native re-entry and coroutine bodies cannot because an outer/resumer
        // thread may live in a Rust frame outside this collector's root set.
        //
        // `take_gc_request` is consumed only when active collection is legal, leaving a
        // request raised inside a nested coroutine pending until control returns to a root
        // dispatch. A request consumed but then aborted under memory pressure is
        // best-effort lost — the cap check below is the real backstop, and re-arming would
        // just retry-loop a failing collect.
        //
        // This block deliberately stays per-instruction: collection timing is
        // semantically load-bearing for weak tables under the generational
        // heap (see SAFEPOINT_INTERVAL).
        if mode.may_collect_active() {
            let requested = heap.take_gc_request();
            // Routine pacing (`gc_debt_due`) keeps the heap lean as it allocates; the cap
            // check is the backstop that still tries to reclaim before failing when the live
            // set itself approaches the ceiling. Coalesced so they never collect twice in one
            // step. Each completed cycle re-arms the debt threshold (`note_collection`).
            if requested || heap.gc_debt_due() || heap.over_memory_cap() || heap.gc_stress_collect()
            {
                // A taken-out count other than one means the dispatch root
                // set is inconsistent — collecting now could free live
                // objects. That is an internal invariant breach, not a tenant
                // error: panic into the containment layer, which poisons the
                // VM (PanicPoison) instead of handing the tenant a catchable
                // error on a heap of unknown integrity.
                assert!(
                    heap.taken_out_thread_count() == 1,
                    "internal active GC root mismatch"
                );
                crate::gc::collect_active(heap, thread);
            }
        }
        // Still over the cap after any reclamation: stop with a catchable error, before
        // the process backstop.
        if heap.over_memory_cap() {
            return Err(err_memory_limit());
        }
        let (active, base, closure, pc, proto, instr) = match profiled_instruction {
            Some(instruction) => instruction,
            None => active_instruction(heap, thread)?,
        };

        // The batched safepoint: cancellation, logical deadline, and the
        // preemption quantum, every SAFEPOINT_INTERVAL instructions. The
        // instruction is fetched but not yet executed, so a preemption here
        // resumes by refetching the same pc.
        if entry_safepoint || since_safepoint >= SAFEPOINT_INTERVAL {
            if let Some(step) = batched_safepoint(heap, mode, since_safepoint)? {
                return Ok(step);
            }
            since_safepoint = 0;
            entry_safepoint = false;
        }
        since_safepoint += 1;

        let mut next_pc = pc + 1;
        let a = base + u32::from(instr.a);
        let b = base + u32::from(instr.b);
        let c = base + u32::from(instr.c);

        match instr.opcode {
            Opcode::Coverage => {
                if let Some(proto) = heap.proto_mut(proto) {
                    proto.hit_coverage(pc);
                }
            }

            Opcode::Nop | Opcode::Break | Opcode::Capture | Opcode::PrepVarargs => {}

            // FASTCALL1: under safeenv, dispatch a hot pure builtin straight
            // from the argument register to the fallback CALL's result
            // register, skipping the GETIMPORT+CALL window entirely
            // (upstream's `cl->env->safeenv` fast path). Any non-conforming
            // shape falls through to the correct fallback.
            Opcode::FastCall1 => {
                if let Some(skip_to) = fastcall1(heap, thread, proto, &instr, pc, base, closure) {
                    next_pc = skip_to;
                }
            }
            // The remaining FASTCALL forms are builtin-dispatch optimizations
            // with a GETIMPORT+CALL fallback; treating them as no-ops runs
            // that correct fallback path.
            Opcode::FastCall | Opcode::FastCall2 | Opcode::FastCall2K | Opcode::FastCall3 => {}

            Opcode::LoadNil => thread.stacks.set(a, RawValue::Nil),
            Opcode::LoadB => {
                thread.stacks.set(a, RawValue::Boolean(instr.b != 0));
                if instr.c != 0 {
                    next_pc = jump_to(heap, proto, pc)?;
                }
            }
            Opcode::LoadN => thread.stacks.set(a, RawValue::Number(f64::from(instr.d))),
            Opcode::LoadK => {
                let v = constant(heap, proto, d_index(&instr)?)?;
                thread.stacks.set(a, v);
            }
            Opcode::LoadKx => {
                let idx = instr.aux.ok_or_else(|| err("LOADKX missing aux"))?;
                let v = constant(heap, proto, idx)?;
                thread.stacks.set(a, v);
            }
            Opcode::Move => {
                let v = thread.stacks.get(b);
                thread.stacks.set(a, v);
            }

            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Mod
            | Opcode::Pow
            | Opcode::IDiv => {
                let lhs = thread.stacks.get(b);
                let rhs = thread.stacks.get(c);
                let v = arith(heap, thread, instr.opcode, lhs, rhs)?;
                thread.stacks.set(a, v);
            }
            Opcode::AddK
            | Opcode::SubK
            | Opcode::MulK
            | Opcode::DivK
            | Opcode::ModK
            | Opcode::PowK
            | Opcode::IDivK => {
                let lhs = thread.stacks.get(b);
                let rhs = constant(heap, proto, u32::from(instr.c))?;
                let v = arith(heap, thread, instr.opcode, lhs, rhs)?;
                thread.stacks.set(a, v);
            }
            Opcode::SubRk | Opcode::DivRk => {
                let lhs = constant(heap, proto, u32::from(instr.b))?;
                let rhs = thread.stacks.get(c);
                let v = arith(heap, thread, instr.opcode, lhs, rhs)?;
                thread.stacks.set(a, v);
            }
            Opcode::Minus => {
                let operand = thread.stacks.get(b);
                let v = if let Some(v) = vmutils::negate(operand) {
                    v
                } else if let RawValue::Vector(components) = operand {
                    // `-vector` negates each component (`lvmexecute.cpp` `LOP_UNM`).
                    RawValue::Vector([-components[0], -components[1], -components[2]])
                } else if let Some(n) = coerce_number(heap, operand) {
                    // A string operand coerces before the metamethod, as binary
                    // arithmetic does (`luaV_doarithimpl` runs `luaV_tonumber`).
                    RawValue::Number(-n)
                } else {
                    // `__unm` receives the operand as both arguments.
                    arith_meta(heap, thread, MetaEvent::Unm, operand, operand)?
                };
                thread.stacks.set(a, v);
            }
            Opcode::Not => {
                let v = RawValue::Boolean(!vmutils::truthy(thread.stacks.get(b)));
                thread.stacks.set(a, v);
            }

            Opcode::And => {
                let lhs = thread.stacks.get(b);
                let v = if vmutils::truthy(lhs) {
                    thread.stacks.get(c)
                } else {
                    lhs
                };
                thread.stacks.set(a, v);
            }
            Opcode::Or => {
                let lhs = thread.stacks.get(b);
                let v = if vmutils::truthy(lhs) {
                    lhs
                } else {
                    thread.stacks.get(c)
                };
                thread.stacks.set(a, v);
            }
            Opcode::AndK => {
                let lhs = thread.stacks.get(b);
                let v = if vmutils::truthy(lhs) {
                    constant(heap, proto, u32::from(instr.c))?
                } else {
                    lhs
                };
                thread.stacks.set(a, v);
            }
            Opcode::OrK => {
                let lhs = thread.stacks.get(b);
                let v = if vmutils::truthy(lhs) {
                    lhs
                } else {
                    constant(heap, proto, u32::from(instr.c))?
                };
                thread.stacks.set(a, v);
            }

            Opcode::Jump | Opcode::JumpBack | Opcode::JumpX => next_pc = jump_to(heap, proto, pc)?,
            Opcode::JumpIf => {
                if vmutils::truthy(thread.stacks.get(a)) {
                    next_pc = jump_to(heap, proto, pc)?;
                }
            }
            Opcode::JumpIfNot => {
                if !vmutils::truthy(thread.stacks.get(a)) {
                    next_pc = jump_to(heap, proto, pc)?;
                }
            }
            Opcode::JumpIfEq | Opcode::JumpIfNotEq => {
                let lhs = thread.stacks.get(a);
                let rhs = thread.stacks.get(base + aux0(&instr)?);
                let eq = values_equal(heap, thread, lhs, rhs)?;
                if eq == (instr.opcode == Opcode::JumpIfEq) {
                    next_pc = jump_to(heap, proto, pc)?;
                }
            }
            Opcode::JumpIfLe | Opcode::JumpIfNotLe => {
                let lhs = thread.stacks.get(a);
                let rhs = thread.stacks.get(base + aux0(&instr)?);
                let le = less_equal_op(heap, thread, lhs, rhs)?;
                if le == (instr.opcode == Opcode::JumpIfLe) {
                    next_pc = jump_to(heap, proto, pc)?;
                }
            }
            Opcode::JumpIfLt | Opcode::JumpIfNotLt => {
                let lhs = thread.stacks.get(a);
                let rhs = thread.stacks.get(base + aux0(&instr)?);
                let lt = less_than_op(heap, thread, lhs, rhs)?;
                if lt == (instr.opcode == Opcode::JumpIfLt) {
                    next_pc = jump_to(heap, proto, pc)?;
                }
            }
            Opcode::JumpXEqKNil | Opcode::JumpXEqKB | Opcode::JumpXEqKN | Opcode::JumpXEqKS => {
                if jump_xeqk(heap, thread, proto, base, &instr)? {
                    next_pc = jump_to(heap, proto, pc)?;
                }
            }

            Opcode::ForNPrep => next_pc = for_nprep(heap, thread, proto, base, &instr, pc)?,
            Opcode::ForNLoop => next_pc = for_nloop(heap, thread, proto, base, &instr, pc)?,

            // The `*_INEXT`/`*_NEXT` preludes follow a `pairs`/`ipairs`/`next`
            // call, which already left a real iterator function in `R[A]`, so
            // they just jump to their `FORGLOOP` (the register fast paths are an
            // optimization skipped here). The generic `FORGPREP` must also handle a
            // bare table or an `__iter` object.
            Opcode::ForGPrepInext | Opcode::ForGPrepNext => {
                next_pc = jump_to(heap, proto, pc)?;
            }
            Opcode::ForGPrep => next_pc = for_gprep(heap, thread, proto, base, &instr, pc)?,
            Opcode::ForGLoop => next_pc = for_gloop(heap, thread, proto, base, &instr, pc)?,

            Opcode::NewClosure => {
                let child = child_proto(heap, proto, d_index(&instr)?)?;
                let env = active_environment(heap, thread);
                let cl = make_closure(heap, child, env)?;
                // Place the closure in its register *before* binding captures: a
                // self-recursive `local function` captures its own register by value
                // (`LOP_CAPTURE VAL`), so the value read at capture time must already be
                // this closure (upstream sets `ra` first). Binding first would snapshot the
                // stale register and the recursive upvalue would point at garbage.
                thread.stacks.set(a, RawValue::Function(cl));
                next_pc = pc + 1 + bind_captures(heap, thread, closure, proto, base, cl, pc)?;
            }
            Opcode::DupClosure => {
                let RuntimeConstant::Proto(child) = constant_raw(heap, proto, d_index(&instr)?)?
                else {
                    return Err(err("DUPCLOSURE constant is not a closure"));
                };
                let env = active_environment(heap, thread);
                let cl = make_closure(heap, child, env)?;
                // See `NewClosure`: the register must hold the closure before captures bind,
                // so a by-value self-capture (recursive `local function`) snapshots itself.
                thread.stacks.set(a, RawValue::Function(cl));
                next_pc = pc + 1 + bind_captures(heap, thread, closure, proto, base, cl, pc)?;
            }

            Opcode::GetUpval => {
                let v = upval_get(heap, thread, closure, instr.b)?;
                thread.stacks.set(a, v);
            }
            Opcode::SetUpval => {
                let v = thread.stacks.get(a);
                upval_set(heap, thread, closure, instr.b, v)?;
            }
            Opcode::CloseUpvals => close_upvals_from(heap, thread, a),

            Opcode::NewTable => {
                let table = heap
                    .alloc_table(LuaTable::new())
                    .ok_or_else(|| err_memory("out of memory allocating table"))?;
                thread.stacks.set(a, RawValue::Table(table));
            }
            Opcode::DupTable => {
                let RuntimeConstant::Table(shape) = constant_raw(heap, proto, d_index(&instr)?)?
                else {
                    return Err(err("DUPTABLE constant is not a table template"));
                };
                let table = make_table(heap, &shape)?;
                thread.stacks.set(a, RawValue::Table(table));
            }
            Opcode::SetList => set_list(heap, thread, base, &instr)?,

            // Index/newindex follow `__index`/`__newindex`; the raw fast path is
            // inside. (The inline-cache `C` slot is a later perf concern.)
            Opcode::GetTable => {
                let table = thread.stacks.get(b);
                let key = thread.stacks.get(c);
                let v = index_value(heap, thread, table, key)?;
                thread.stacks.set(a, v);
            }
            Opcode::SetTable => {
                let table = thread.stacks.get(b);
                let key = thread.stacks.get(c);
                let value = thread.stacks.get(a);
                newindex_value(heap, thread, table, key, value)?;
            }
            Opcode::GetTableN => {
                let table = thread.stacks.get(b);
                let v = index_value(heap, thread, table, array_key(instr.c))?;
                thread.stacks.set(a, v);
            }
            Opcode::SetTableN => {
                let table = thread.stacks.get(b);
                let value = thread.stacks.get(a);
                newindex_value(heap, thread, table, array_key(instr.c), value)?;
            }
            Opcode::GetTableKs => {
                let table = thread.stacks.get(b);
                let key = constant(heap, proto, aux0(&instr)?)?;
                let v = index_value(heap, thread, table, key)?;
                thread.stacks.set(a, v);
            }
            Opcode::SetTableKs => {
                let table = thread.stacks.get(b);
                let value = thread.stacks.get(a);
                let key = constant(heap, proto, aux0(&instr)?)?;
                newindex_value(heap, thread, table, key, value)?;
            }

            // Global and import access resolve names against the thread's globals.
            Opcode::GetImport => {
                let idx = d_index(&instr)?;
                // Under the safeenv-frozen globals (and a closure with no
                // setfenv environment), an import is immutable: resolve once
                // per import site and reuse the cached value.
                let cacheable = safeenv_active(heap, thread, closure);
                let cached = if cacheable {
                    heap.proto(proto).and_then(|p| p.cached_import(idx))
                } else {
                    None
                };
                let v = match cached {
                    Some(v) => v,
                    None => {
                        let RuntimeConstant::Import(id) = constant_raw(heap, proto, idx)? else {
                            return Err(err("GETIMPORT constant is not an import"));
                        };
                        let v = resolve_import(heap, thread, proto, id)?;
                        if cacheable {
                            if let Some(p) = heap.proto_mut(proto) {
                                p.cache_import(idx, v);
                            }
                            // The proto may be old and the value young: record
                            // the mutation for the next minor collection.
                            crate::gc::remember(heap, crate::gc::GcRef::Proto(proto.index()));
                        }
                        v
                    }
                };
                thread.stacks.set(a, v);
            }
            Opcode::GetGlobal => {
                let key = constant(heap, proto, aux0(&instr)?)?;
                let globals =
                    active_environment(heap, thread).map_or(RawValue::Nil, RawValue::Table);
                let v = index_value(heap, thread, globals, key)?;
                thread.stacks.set(a, v);
            }
            Opcode::SetGlobal => {
                let key = constant(heap, proto, aux0(&instr)?)?;
                let value = thread.stacks.get(a);
                let globals =
                    active_environment(heap, thread).map_or(RawValue::Nil, RawValue::Table);
                newindex_value(heap, thread, globals, key, value)?;
            }

            Opcode::Length => {
                let operand = thread.stacks.get(b);
                let v = length_of(heap, thread, operand)?;
                thread.stacks.set(a, v);
            }
            Opcode::Concat => {
                let v = concat_range(heap, thread, b, c)?;
                thread.stacks.set(a, v);
            }

            Opcode::GetVarargs => {
                // `...`: copy the frame's captured varargs into `R[A..]`. A multret
                // request (`B == 0`) copies all of them and lowers the live top to
                // match; a fixed `B - 1` count copies that many, nil-filling any
                // shortfall. The fixed targets are within the function's register
                // window, but a multret copy can run past it, so grow first. Each
                // value is read by index — the borrow of `varargs` ends before each
                // `set` — so there is no per-instruction clone of the whole vector.
                if instr.b == 0 {
                    let n = u32::try_from(
                        thread.call_stack[active]
                            .frame()
                            .ok_or_else(|| err("protected boundary has no varargs"))?
                            .varargs
                            .len(),
                    )
                    .unwrap_or(u32::MAX);
                    prepare_result_copy(heap, n as usize, "vararg")?;
                    thread
                        .stacks
                        .ensure(a.saturating_add(n))
                        .map_err(|_| err_register_stack_oom())?;
                    for i in 0..n {
                        let value = thread.call_stack[active]
                            .frame()
                            .ok_or_else(|| err("protected boundary has no varargs"))?
                            .varargs
                            .get(i as usize)
                            .copied()
                            .unwrap_or(RawValue::Nil);
                        thread.stacks.set(a + i, value);
                    }
                    thread.top = a.saturating_add(n);
                } else {
                    for i in 0..(u32::from(instr.b) - 1) {
                        let value = thread.call_stack[active]
                            .frame()
                            .ok_or_else(|| err("protected boundary has no varargs"))?
                            .varargs
                            .get(i as usize)
                            .copied()
                            .unwrap_or(RawValue::Nil);
                        thread.stacks.set(a + i, value);
                    }
                }
            }

            // `obj:method(...)`: load the method into `R[A]` (following `__index`)
            // and the receiver into `R[A+1]`; the next `CALL` runs it. Upstream
            // `LOP_NAMECALL` consults `__namecall` only for non-table receivers
            // (userdata or a basic-type metatable); a table always takes this
            // `__index` lookup. Non-table `__namecall` remains unsupported.
            Opcode::NameCall => {
                let object = thread.stacks.get(b);
                let method_name = constant(heap, proto, aux0(&instr)?)?;
                let method = namecall_method(heap, thread, object, method_name)?;
                thread.stacks.set(a + 1, object);
                thread.stacks.set(a, method);
            }

            Opcode::Call => {
                // Set the caller's resume point only after `precall` succeeds, so
                // a builtin that raises during the call leaves `savedpc` at this
                // CALL — its source line is the call site the error reports. The
                // same resume point applies to a `coroutine.yield`: the resume
                // continues at the instruction after this CALL.
                let step = precall(heap, thread, base, &instr, mode.may_preempt())?;
                if !matches!(step, PrecallStep::Preempt | PrecallStep::WaitForInFlight) {
                    thread.call_stack[active]
                        .frame_mut()
                        .ok_or_else(|| err("protected boundary reached as an executable frame"))?
                        .savedpc = pc + 1;
                }
                match step {
                    PrecallStep::Done => {}
                    PrecallStep::Preempt => return Ok(Step::Preempt),
                    PrecallStep::Yield(values) => return Ok(Step::Yield(values)),
                    PrecallStep::WaitForInFlight => return Ok(Step::Yield(Vec::new())),
                    // An async host call is pending: unwind to the async driver,
                    // which awaits the future and resumes here at `savedpc` once
                    // it has placed the result at the call's result register. Record
                    // this CALL's pc so an async failure locates at the call site,
                    // not the (already advanced) resume `savedpc`.
                    PrecallStep::Suspend(mut call) => {
                        call.set_dispatch_call_pc(pc);
                        return Ok(Step::Suspend(call));
                    }
                    PrecallStep::SuspendRequire(mut require) => {
                        require.set_dispatch_call_pc(pc);
                        return Ok(Step::SuspendRequire(require));
                    }
                }
                continue;
            }
            Opcode::Return => {
                if let Some((result_base, count)) = return_op(heap, thread, floor, base, &instr)? {
                    return Ok(Step::Return(collect_stack_results(
                        heap,
                        thread,
                        result_base,
                        count,
                        "return",
                    )?));
                }
                continue;
            }

            other => {
                return Err(err(format!("unsupported opcode {other:?}")));
            }
        }

        thread.call_stack[active]
            .frame_mut()
            .ok_or_else(|| err("protected boundary reached as an executable frame"))?
            .savedpc = next_pc;
    }
}

/// `FORNPREP`: the control registers are `R[A]=limit`, `R[A+1]=step`,
/// `R[A+2]=index` — where `R[A+2]` is also the loop variable the body reads. If
/// the range is already empty, jump past the loop.
fn for_nprep(
    heap: &Heap,
    thread: &mut Thread,
    proto: RawGc<Proto>,
    base: u32,
    instr: &Instruction,
    pc: usize,
) -> Exec<usize> {
    let a = base + u32::from(instr.a);
    let limit = as_num(heap, thread.stacks.get(a), "limit")?;
    let step = as_num(heap, thread.stacks.get(a + 1), "step")?;
    let index = as_num(heap, thread.stacks.get(a + 2), "initial value")?;
    // Write the coerced numbers back into the control registers, as
    // `luaV_prepareFORN` does: `FORNLOOP` then reads plain numbers and the loop
    // variable is a number from the first iteration even for string bounds.
    thread.stacks.set(a, RawValue::Number(limit));
    thread.stacks.set(a + 1, RawValue::Number(step));
    thread.stacks.set(a + 2, RawValue::Number(index));
    // A zero step is not special-cased: it takes the `step > 0` false branch and
    // the `limit <= index` test decides entry, matching upstream `FORNPREP` so
    // NaN and zero-step behavior stay identical to `FORNLOOP`.
    let enters = if step > 0.0 {
        index <= limit
    } else {
        limit <= index
    };
    if enters {
        Ok(pc + 1)
    } else {
        jump_to(heap, proto, pc)
    }
}

/// `FORNLOOP`: advance `R[A+2]` by `R[A+1]` and write it back, then if still
/// within `R[A]` jump to the loop body. Upstream writes the advanced index
/// unconditionally — including the iteration that overshoots and exits — so the
/// loop register holds the same value on exit.
fn for_nloop(
    heap: &Heap,
    thread: &mut Thread,
    proto: RawGc<Proto>,
    base: u32,
    instr: &Instruction,
    pc: usize,
) -> Exec<usize> {
    let a = base + u32::from(instr.a);
    let limit = as_num(heap, thread.stacks.get(a), "limit")?;
    let step = as_num(heap, thread.stacks.get(a + 1), "step")?;
    let index = as_num(heap, thread.stacks.get(a + 2), "index")? + step;
    thread.stacks.set(a + 2, RawValue::Number(index));
    let continues = if step > 0.0 {
        index <= limit
    } else {
        limit <= index
    };
    if continues {
        jump_to(heap, proto, pc)
    } else {
        Ok(pc + 1)
    }
}

/// `FORGPREP` (generic): readies a generic-for whose loop expression `R[A]` may
/// not be a function. A function is left for `FORGLOOP` to call; an `__iter`
/// object is replaced by the iterator triple `__iter` returns; a `__call` value
/// is left for `FORGLOOP` to invoke; a bare table is set up to iterate with
/// `next` (`R[A]=next, R[A+1]=t, R[A+2]=nil`); anything else raises. Then jump to
/// the `FORGLOOP`.
fn for_gprep(
    heap: &mut Heap,
    thread: &mut Thread,
    proto: RawGc<Proto>,
    base: u32,
    instr: &Instruction,
    pc: usize,
) -> Exec<usize> {
    let a = base + u32::from(instr.a);
    let target = jump_to(heap, proto, pc)?;
    let iterator = thread.stacks.get(a);
    if matches!(iterator, RawValue::Function(_)) {
        return Ok(target);
    }
    if let Some(handler) = tm::get_metamethod(heap, iterator, MetaEvent::Iter)? {
        let results = call_value(heap, thread, handler, &[iterator])?;
        thread
            .stacks
            .set(a, results.first().copied().unwrap_or(RawValue::Nil));
        thread
            .stacks
            .set(a + 1, results.get(1).copied().unwrap_or(RawValue::Nil));
        thread
            .stacks
            .set(a + 2, results.get(2).copied().unwrap_or(RawValue::Nil));
        return Ok(target);
    }
    if tm::get_metamethod(heap, iterator, MetaEvent::Call)?.is_some() {
        // A callable value: `FORGLOOP` invokes it through its `__call`.
        return Ok(target);
    }
    if matches!(iterator, RawValue::Table(_)) {
        let next = heap
            .alloc_builtin(crate::builtins::Builtin::Next)
            .ok_or_else(|| err_memory("out of memory creating an iterator"))?;
        thread.stacks.set(a + 1, iterator);
        thread.stacks.set(a, RawValue::Function(next));
        thread.stacks.set(a + 2, RawValue::Nil);
        return Ok(target);
    }
    Err(iter_error(iterator))
}

fn iter_error(value: RawValue) -> crate::call::RaisedError {
    let type_name = core::str::from_utf8(crate::builtins::type_name(value)).unwrap_or("value");
    err(format!("attempt to iterate over a {type_name} value"))
}

/// `FORGLOOP` (generic path): calls the iterator `R[A](R[A+1], R[A+2])`, writes
/// its first `aux` results into the loop variables `R[A+3..]` and the control
/// `R[A+2]`, and loops while the first result is non-`nil` (`luaV` generic-for).
fn for_gloop(
    heap: &mut Heap,
    thread: &mut Thread,
    proto: RawGc<Proto>,
    base: u32,
    instr: &Instruction,
    pc: usize,
) -> Exec<usize> {
    let a = base + u32::from(instr.a);
    let iterator = thread.stacks.get(a);
    let state = thread.stacks.get(a + 1);
    let control = thread.stacks.get(a + 2);
    let nvars = aux0(instr)? & FORGLOOP_VARS_MASK;
    // Inline iterator fast path: `pairs`/`ipairs` loops drive the `next`/
    // `inext` builtins over a table — step the table directly instead of
    // building a call (and a result vector) per iteration. Identity is the
    // iterator *value* (a closure over the native proto), so a shadowed
    // global that still hands back the real builtin stays correct, and
    // anything else falls through to the general call.
    if let (RawValue::Function(cl), RawValue::Table(table)) = (iterator, state)
        && let Some(native) = heap
            .closure(cl)
            .map(|c| c.proto)
            .and_then(|p| heap.proto(p))
            .and_then(|p| p.native)
    {
        match native {
            crate::builtins::Builtin::Next => {
                let step = heap
                    .table(table)
                    .ok_or_else(|| err("'next' on a non-resident table"))?
                    .next(control);
                let (key, value) = match step {
                    crate::table::NextStep::Pair(key, value) => (key, value),
                    crate::table::NextStep::Done => (RawValue::Nil, RawValue::Nil),
                    crate::table::NextStep::InvalidKey => {
                        return Err(err("invalid key to 'next'"));
                    }
                };
                if nvars >= 1 {
                    thread.stacks.set(a + 3, key);
                }
                if nvars >= 2 {
                    thread.stacks.set(a + 4, value);
                }
                for i in 2..nvars {
                    thread.stacks.set(a + 3 + i, RawValue::Nil);
                }
                thread.stacks.set(a + 2, key);
                return if matches!(key, RawValue::Nil) {
                    Ok(pc + 1)
                } else {
                    jump_to(heap, proto, pc)
                };
            }
            crate::builtins::Builtin::INext => {
                let index = match control {
                    RawValue::Number(n) => n,
                    RawValue::Integer(i) => i as f64,
                    _ => 0.0,
                };
                let next_index = index + 1.0;
                let value = heap
                    .table(table)
                    .map_or(RawValue::Nil, |t| t.get(RawValue::Number(next_index)));
                let done = matches!(value, RawValue::Nil);
                let key = if done {
                    RawValue::Nil
                } else {
                    RawValue::Number(next_index)
                };
                if nvars >= 1 {
                    thread.stacks.set(a + 3, key);
                }
                if nvars >= 2 {
                    thread.stacks.set(a + 4, value);
                }
                for i in 2..nvars {
                    thread.stacks.set(a + 3 + i, RawValue::Nil);
                }
                thread.stacks.set(a + 2, key);
                return if done {
                    Ok(pc + 1)
                } else {
                    jump_to(heap, proto, pc)
                };
            }
            _ => {}
        }
    }
    let results = call_value(heap, thread, iterator, &[state, control])?;
    let first = results.first().copied().unwrap_or(RawValue::Nil);
    for i in 0..nvars {
        let value = results.get(i as usize).copied().unwrap_or(RawValue::Nil);
        thread.stacks.set(a + 3 + i, value);
    }
    // The first variable is also the next control value.
    thread.stacks.set(a + 2, first);
    if matches!(first, RawValue::Nil) {
        Ok(pc + 1)
    } else {
        jump_to(heap, proto, pc)
    }
}

fn jump_xeqk(
    heap: &Heap,
    thread: &Thread,
    proto: RawGc<Proto>,
    base: u32,
    instr: &Instruction,
) -> Exec<bool> {
    let lhs = thread.stacks.get(base + u32::from(instr.a));
    let aux = aux0(instr)?;
    let not = (aux & JUMPX_K_NOT_BIT) != 0;
    let kidx = aux & JUMPX_K_INDEX_MASK;
    let rhs = match instr.opcode {
        Opcode::JumpXEqKNil => RawValue::Nil,
        Opcode::JumpXEqKB => RawValue::Boolean((kidx & 1) != 0),
        _ => constant(heap, proto, kidx)?,
    };
    Ok(vmutils::raw_equal(lhs, rhs) != not)
}

/// Allocates a closure over `proto` with no upvalue cells bound yet;
/// [`bind_captures`] fills them from the `CAPTURE` pseudo-instructions.
fn make_closure(
    heap: &mut Heap,
    proto: RawGc<Proto>,
    env: Option<RawGc<marker::Table>>,
) -> Exec<RawGc<marker::Closure>> {
    let mut closure = Closure::new(proto);
    closure.env = env;
    heap.alloc_closure(closure)
        .ok_or_else(|| err_memory("out of memory allocating closure"))
}

/// Binds the upvalue cells of the just-created closure `new_closure` from the
/// `CAPTURE` pseudo-instructions that follow `NEWCLOSURE`/`DUPCLOSURE` at `pc`.
/// Returns how many `CAPTURE` words to skip. `VAL` snapshots a register, `REF`
/// shares an open cell over the slot, and `UPVAL` re-captures one of the running
/// closure's own upvalues.
fn bind_captures(
    heap: &mut Heap,
    thread: &mut Thread,
    running: RawGc<marker::Closure>,
    proto: RawGc<Proto>,
    base: u32,
    new_closure: RawGc<marker::Closure>,
    pc: usize,
) -> Exec<usize> {
    let new_proto = heap
        .closure(new_closure)
        .map(|c| c.proto)
        .ok_or_else(|| err("closure not resident"))?;
    let count = heap
        .proto(new_proto)
        .map_or(0, |p| usize::from(p.num_upvalues));
    for i in 0..count {
        let capture = heap
            .proto(proto)
            .and_then(|p| p.instruction(pc + 1 + i))
            .ok_or_else(|| err("NEWCLOSURE missing a CAPTURE word"))?;
        let source = u32::from(capture.b);
        let cell = match capture.capture_type() {
            Some(CaptureType::Val) => {
                let value = thread.stacks.get(base + source);
                heap.alloc_upval(UpVal::Closed(value))
                    .ok_or_else(|| err_memory("out of memory (upvalue)"))?
            }
            Some(CaptureType::Ref) => open_upval(heap, thread, base + source)?,
            Some(CaptureType::Upval) => heap
                .closure(running)
                .and_then(|c| c.upvals.get(source as usize).copied())
                .ok_or_else(|| err("CAPTURE UPVAL index out of range"))?,
            None => return Err(err("malformed CAPTURE")),
        };
        heap.closure_mut(new_closure)
            .ok_or_else(|| err("closure not resident"))?
            .upvals
            .push(cell);
    }
    // Charge the closure's populated upvalue buffer: the arena counts only the
    // `Closure` struct header, not this heap-allocated vector, and closures are
    // created unboundedly (a `NEWCLOSURE` in a loop), so it counts against the cap.
    let upval_bytes = heap.closure(new_closure).map_or(0, |c| {
        c.upvals.capacity() * std::mem::size_of::<RawGc<UpVal>>()
    });
    heap.meter().charge(upval_bytes);
    Ok(count)
}

/// Finds the open upvalue already covering `slot`, or allocates one and records
/// it on the thread, so reference captures of the same slot share a cell.
fn open_upval(heap: &mut Heap, thread: &mut Thread, slot: u32) -> Exec<RawGc<UpVal>> {
    for &handle in &thread.open_upvals {
        if let Some(UpVal::Open { slot: existing, .. }) = heap.upval(handle)
            && *existing == slot
        {
            return Ok(handle);
        }
    }
    let id = thread
        .id
        .ok_or_else(|| err("thread has no heap identity"))?;
    let cell = heap
        .alloc_upval(UpVal::Open { thread: id, slot })
        .ok_or_else(|| err_memory("out of memory (upvalue)"))?;
    thread.open_upvals.push(cell);
    Ok(cell)
}

/// Reads upvalue `index` of `closure`: an open cell reads the register of the
/// thread it was captured over — the active thread itself, or a parked coroutine
/// reached through the heap — and a closed cell reads its owned value.
fn upval_get(
    heap: &Heap,
    thread: &Thread,
    closure: RawGc<marker::Closure>,
    index: u8,
) -> Exec<RawValue> {
    let handle = upval_handle(heap, closure, index)?;
    let open = match heap
        .upval(handle)
        .ok_or_else(|| err("upvalue not resident"))?
    {
        UpVal::Open { thread, slot, .. } => (*thread, *slot),
        UpVal::Closed(value) => return Ok(*value),
    };
    Ok(open_register(heap, thread, open.0, open.1))
}

/// Reads register `slot` of the thread an open upvalue was captured over. The
/// running thread is `active` (taken out of the arena), so an upvalue over it
/// reads `active` directly; one over another coroutine reads that parked thread
/// through the heap, validating the `{heap, generation}` guard baked into the
/// owner handle — a stale or foreign owner yields `nil`.
fn open_register(
    heap: &Heap,
    active: &Thread,
    owner: RawGc<marker::Thread>,
    slot: u32,
) -> RawValue {
    if Some(owner) == active.id {
        active.stacks.get(slot)
    } else {
        heap.thread(owner)
            .map_or(RawValue::Nil, |t| t.stacks.get(slot))
    }
}

/// Writes upvalue `index` of `closure`: an open cell writes the register of its
/// owning thread (the active thread, or a parked coroutine), a closed cell
/// updates its owned value.
fn upval_set(
    heap: &mut Heap,
    thread: &mut Thread,
    closure: RawGc<marker::Closure>,
    index: u8,
    value: RawValue,
) -> Exec<()> {
    let handle = upval_handle(heap, closure, index)?;
    let open = match heap
        .upval(handle)
        .ok_or_else(|| err("upvalue not resident"))?
    {
        UpVal::Open { thread, slot, .. } => Some((*thread, *slot)),
        UpVal::Closed(_) => None,
    };
    match open {
        Some((owner, slot)) => {
            if Some(owner) == thread.id {
                thread.stacks.set(slot, value);
            } else if let Some(owner) = heap.thread_mut(owner) {
                owner.stacks.set(slot, value);
            }
        }
        None => {
            if let Some(cell) = heap.upval_mut(handle) {
                *cell = UpVal::Closed(value);
            }
        }
    }
    Ok(())
}

fn upval_handle(heap: &Heap, closure: RawGc<marker::Closure>, index: u8) -> Exec<RawGc<UpVal>> {
    heap.closure(closure)
        .and_then(|c| c.upvals.get(usize::from(index)).copied())
        .ok_or_else(|| err("upvalue index out of range"))
}

/// Closes every open upvalue at or above `from_slot`, copying its live register
/// into the cell so the value outlives the frame (`luaF_close`). Driven by
/// `CLOSEUPVALS`, a returning frame, and an unwind.
pub fn close_upvals_from(heap: &mut Heap, thread: &mut Thread, from_slot: u32) {
    let mut i = 0;
    while i < thread.open_upvals.len() {
        let handle = thread.open_upvals[i];
        let slot = match heap.upval(handle) {
            Some(UpVal::Open { slot, .. }) => Some(*slot),
            _ => None,
        };
        match slot {
            Some(slot) if slot >= from_slot => {
                let value = thread.stacks.get(slot);
                if let Some(cell) = heap.upval_mut(handle) {
                    *cell = UpVal::Closed(value);
                }
                thread.open_upvals.swap_remove(i);
            }
            _ => i += 1,
        }
    }
}

/// Applies an arithmetic opcode, falling back to the binary metamethod
/// (`__add`, …) when a raw operand is not a number.
fn arith(
    heap: &mut Heap,
    thread: &mut Thread,
    opcode: Opcode,
    lhs: RawValue,
    rhs: RawValue,
) -> Exec<RawValue> {
    let (op, event) = arith_kinds(opcode);
    if let Some(v) = vmutils::arith(op, lhs, rhs) {
        return Ok(v);
    }
    // String operands coerce to numbers before the metamethod (luaV_tonumber).
    if let (Some(a), Some(b)) = (coerce_number(heap, lhs), coerce_number(heap, rhs))
        && let Some(v) = vmutils::arith(op, RawValue::Number(a), RawValue::Number(b))
    {
        return Ok(v);
    }
    // Vector operands: upstream's inline fast paths and vector-metatable
    // arithmetic (`lvmexecute.cpp`) before the generic metamethod/error.
    if let Some(v) = vector_arith(heap, op, lhs, rhs) {
        return Ok(v);
    }
    arith_meta(heap, thread, event, lhs, rhs)
}

/// Applies an arithmetic operator to vector operands, matching upstream's vector
/// fast paths and vector-metatable arithmetic (`lvmexecute.cpp`): `+`/`-` require
/// two vectors (componentwise); `*`, `/`, and `//` accept vector⊕vector
/// (componentwise) or a vector with a scalar — a number or numeric string,
/// `luaV_tonumber`-coerced — broadcast on either side. All arithmetic runs in
/// `f32`, and `//` is `floor(a / b)` (`luai_numidiv`). Returns `None` for any
/// unsupported shape (a vector `%`/`^`, or `+`/`-` against a non-vector), leaving
/// the caller's metamethod/error path.
fn vector_arith(heap: &Heap, op: ArithOp, lhs: RawValue, rhs: RawValue) -> Option<RawValue> {
    // `%` and `^` have neither a vector fast path nor a vector metamethod.
    if matches!(op, ArithOp::Mod | ArithOp::Pow) {
        return None;
    }
    let apply = |a: f32, b: f32| -> f32 {
        match op {
            ArithOp::Add => a + b,
            ArithOp::Sub => a - b,
            ArithOp::Mul => a * b,
            ArithOp::Div => a / b,
            ArithOp::IDiv => (a / b).floor(),
            ArithOp::Mod | ArithOp::Pow => unreachable!("excluded above"),
        }
    };
    // The scalar operand of a broadcast, cast to `f32` exactly as upstream casts
    // `nvalue` for the component arithmetic.
    let scalar = |value: RawValue| coerce_number(heap, value).map(|n| n as f32);
    match (lhs, rhs) {
        (RawValue::Vector(a), RawValue::Vector(b)) => Some(RawValue::Vector([
            apply(a[0], b[0]),
            apply(a[1], b[1]),
            apply(a[2], b[2]),
        ])),
        // A scalar broadcast is defined only for `*`, `/`, and `//`; `+`/`-`
        // against a non-vector fall through to the (absent) metamethod and error.
        (RawValue::Vector(a), other)
            if matches!(op, ArithOp::Mul | ArithOp::Div | ArithOp::IDiv) =>
        {
            let s = scalar(other)?;
            Some(RawValue::Vector([
                apply(a[0], s),
                apply(a[1], s),
                apply(a[2], s),
            ]))
        }
        (other, RawValue::Vector(b))
            if matches!(op, ArithOp::Mul | ArithOp::Div | ArithOp::IDiv) =>
        {
            let s = scalar(other)?;
            Some(RawValue::Vector([
                apply(s, b[0]),
                apply(s, b[1]),
                apply(s, b[2]),
            ]))
        }
        _ => None,
    }
}

/// Coerces a value to a number for arithmetic and `for`-bound contexts: a number
/// passes through, a string parses (`luaV_tonumber`), and everything else —
/// including this revision's integers — fails.
fn coerce_number(heap: &Heap, value: RawValue) -> Option<f64> {
    match value {
        RawValue::Number(n) => Some(n),
        RawValue::String(handle) => heap
            .string(handle)
            .and_then(|s| vmutils::str_to_number(s.bytes())),
        _ => None,
    }
}

/// The raw operator and metamethod event an arithmetic opcode dispatches.
fn arith_kinds(opcode: Opcode) -> (ArithOp, MetaEvent) {
    match opcode {
        Opcode::Add | Opcode::AddK => (ArithOp::Add, MetaEvent::Add),
        Opcode::Sub | Opcode::SubK | Opcode::SubRk => (ArithOp::Sub, MetaEvent::Sub),
        Opcode::Mul | Opcode::MulK => (ArithOp::Mul, MetaEvent::Mul),
        Opcode::Div | Opcode::DivK | Opcode::DivRk => (ArithOp::Div, MetaEvent::Div),
        Opcode::Mod | Opcode::ModK => (ArithOp::Mod, MetaEvent::Mod),
        Opcode::Pow | Opcode::PowK => (ArithOp::Pow, MetaEvent::Pow),
        _ => (ArithOp::IDiv, MetaEvent::IDiv),
    }
}

/// Dispatches a binary arithmetic metamethod: tries `event` on the left
/// operand's metatable, then the right's, and calls the handler with both
/// operands. (`__unm` passes the single operand as both arguments.) Absent on
/// both, it raises the arithmetic error.
fn arith_meta(
    heap: &mut Heap,
    thread: &mut Thread,
    event: MetaEvent,
    lhs: RawValue,
    rhs: RawValue,
) -> Exec<RawValue> {
    let handler = match tm::get_metamethod(heap, lhs, event)? {
        Some(handler) => handler,
        None => tm::get_metamethod(heap, rhs, event)?
            .ok_or_else(|| arithmetic_error(event, lhs, rhs))?,
    };
    let results = call_value(heap, thread, handler, &[lhs, rhs])?;
    Ok(results.into_iter().next().unwrap_or(RawValue::Nil))
}

fn arithmetic_error(event: MetaEvent, lhs: RawValue, rhs: RawValue) -> crate::call::RaisedError {
    let op = match event {
        MetaEvent::Add => "add",
        MetaEvent::Sub => "sub",
        MetaEvent::Mul => "mul",
        MetaEvent::Div => "div",
        MetaEvent::Mod => "mod",
        MetaEvent::Pow => "pow",
        MetaEvent::IDiv => "idiv",
        MetaEvent::Unm => "unm",
        _ => "unknown",
    };
    let lhs_type = value_type_name(lhs);
    let rhs_type = value_type_name(rhs);
    let operands = if event == MetaEvent::Unm || lhs_type == rhs_type {
        lhs_type.to_owned()
    } else {
        format!("{lhs_type} and {rhs_type}")
    };
    err(format!(
        "attempt to perform arithmetic ({op}) on {operands}"
    ))
}

/// `lhs < rhs` (`luaV_lessthan`): numbers compare numerically, strings lexically,
/// and two values of the same tag dispatch a matching `__lt`. Numeric operands
/// promote across the integer/number split (Lua 5.3+ semantics); operands of
/// other differing tags raise the order error *before* any metamethod.
///
/// `pub(crate)` so `table.sort`'s default comparator can share the exact `<`
/// semantics, including `__lt` dispatch, that upstream's `lua_lessthan` gives it.
pub fn less_than_op(
    heap: &mut Heap,
    thread: &mut Thread,
    lhs: RawValue,
    rhs: RawValue,
) -> Exec<bool> {
    if let Some(result) = vmutils::less_than(lhs, rhs) {
        return Ok(result);
    }
    if let (RawValue::String(a), RawValue::String(b)) = (lhs, rhs) {
        return Ok(string_cmp(heap, a, b).is_lt());
    }
    if let (Some(a), Some(b)) = (numeric_f64(lhs), numeric_f64(rhs)) {
        return Ok(a < b);
    }
    if !same_tag(lhs, rhs) {
        return Err(order_error("<", lhs, rhs));
    }
    match matching_metamethod(heap, lhs, rhs, MetaEvent::Lt)? {
        Some(handler) => call_compare(heap, thread, handler, lhs, rhs),
        None => Err(order_error("<", lhs, rhs)),
    }
}

/// `lhs <= rhs` (`luaV_lessequal`): like [`less_than_op`], but a missing `__le`
/// falls back to `not (rhs < lhs)` through a matching `__lt`, matching upstream.
fn less_equal_op(heap: &mut Heap, thread: &mut Thread, lhs: RawValue, rhs: RawValue) -> Exec<bool> {
    if let Some(result) = vmutils::less_equal(lhs, rhs) {
        return Ok(result);
    }
    if let (RawValue::String(a), RawValue::String(b)) = (lhs, rhs) {
        return Ok(string_cmp(heap, a, b).is_le());
    }
    if let (Some(a), Some(b)) = (numeric_f64(lhs), numeric_f64(rhs)) {
        return Ok(a <= b);
    }
    if !same_tag(lhs, rhs) {
        return Err(order_error("<=", lhs, rhs));
    }
    if let Some(handler) = matching_metamethod(heap, lhs, rhs, MetaEvent::Le)? {
        return call_compare(heap, thread, handler, lhs, rhs);
    }
    // Fallback: `lhs <= rhs` is `not (rhs < lhs)` via a matching `__lt`.
    match matching_metamethod(heap, rhs, lhs, MetaEvent::Lt)? {
        Some(handler) => Ok(!call_compare(heap, thread, handler, rhs, lhs)?),
        None => Err(order_error("<=", lhs, rhs)),
    }
}

/// Whether two values share a runtime tag (the `ttype(l) == ttype(r)` gate).
fn same_tag(lhs: RawValue, rhs: RawValue) -> bool {
    std::mem::discriminant(&lhs) == std::mem::discriminant(&rhs)
}

/// Coerce a numeric operand (`Integer` or `Number`) to `f64` for ordering.
///
/// Ordering comparisons promote across the integer/number split (Lua 5.3+
/// semantics) so host-supplied integers compare against script number literals;
/// non-numeric operands yield `None` and fall through to the tag/metamethod path.
fn numeric_f64(value: RawValue) -> Option<f64> {
    match value {
        RawValue::Number(number) => Some(number),
        RawValue::Integer(integer) => Some(integer as f64),
        _ => None,
    }
}

fn order_error(op: &str, lhs: RawValue, rhs: RawValue) -> crate::call::RaisedError {
    err(format!(
        "attempt to compare {} {op} {}",
        value_type_name(lhs),
        value_type_name(rhs)
    ))
}

fn value_type_name(value: RawValue) -> &'static str {
    core::str::from_utf8(crate::builtins::type_name(value)).unwrap_or("value")
}

/// The shared ordered/equality metamethod a comparison dispatches: upstream
/// (`get_compTM`/`call_orderTM`) requires *both* operands to carry the same
/// handler — the same metatable, or two raw-equal handler values. A handler on
/// only one side, or differing handlers, count as absent.
fn matching_metamethod(
    heap: &Heap,
    a: RawValue,
    b: RawValue,
    event: MetaEvent,
) -> Exec<Option<RawValue>> {
    let Some(h1) = tm::get_metamethod(heap, a, event)? else {
        return Ok(None);
    };
    let Some(h2) = tm::get_metamethod(heap, b, event)? else {
        return Ok(None);
    };
    Ok(vmutils::raw_equal(h1, h2).then_some(h1))
}

/// Calls a comparison metamethod and returns the truthiness of its first result.
fn call_compare(
    heap: &mut Heap,
    thread: &mut Thread,
    handler: RawValue,
    lhs: RawValue,
    rhs: RawValue,
) -> Exec<bool> {
    let results = call_value(heap, thread, handler, &[lhs, rhs])?;
    Ok(vmutils::truthy(
        results.into_iter().next().unwrap_or(RawValue::Nil),
    ))
}

/// Equality with the `__eq` metamethod (`luaV_equalval`). Primitive and
/// cross-type pairs are decided by raw equality. Two tables or two userdata
/// consult a *matching* `__eq` (same handler on both, per `get_compTM`) — and that
/// handler runs even for the same object — falling back to identity when absent.
fn values_equal(heap: &mut Heap, thread: &mut Thread, lhs: RawValue, rhs: RawValue) -> Exec<bool> {
    let comparable = matches!(
        (lhs, rhs),
        (RawValue::Table(_), RawValue::Table(_)) | (RawValue::Userdata(_), RawValue::Userdata(_))
    );
    if !comparable {
        return Ok(vmutils::raw_equal(lhs, rhs));
    }
    match matching_metamethod(heap, lhs, rhs, MetaEvent::Eq)? {
        Some(handler) => call_compare(heap, thread, handler, lhs, rhs),
        None => Ok(vmutils::raw_equal(lhs, rhs)),
    }
}

/// Byte-wise ordering of two interned strings (Lua `strcmp`).
fn string_cmp(heap: &Heap, a: RawGc<marker::Str>, b: RawGc<marker::Str>) -> std::cmp::Ordering {
    let lhs = heap.string(a).map_or(&[][..], |s| s.bytes());
    let rhs = heap.string(b).map_or(&[][..], |s| s.bytes());
    lhs.cmp(rhs)
}

/// A numeric `for` control value: a number, or a string that parses to one
/// (`FORNPREP` coerces via `luaV_tonumber`). This revision's integers do not
/// qualify.
fn as_num(heap: &Heap, value: RawValue, what: &str) -> Exec<f64> {
    coerce_number(heap, value).ok_or_else(|| {
        err(format!(
            "invalid 'for' {what} (number expected, got {})",
            value_type_name(value)
        ))
    })
}

fn aux0(instr: &Instruction) -> Exec<u32> {
    instr.aux.ok_or_else(|| err("instruction missing aux word"))
}

/// The `D` operand read as a non-negative table index (`LOADK`, `NEWCLOSURE`,
/// `DUPCLOSURE`); negative `D` is reserved for jump offsets, not these opcodes.
fn d_index(instr: &Instruction) -> Exec<u32> {
    u32::try_from(instr.d).map_err(|_| err("operand index is negative"))
}

/// Resolves a branch's target from the proto's precomputed table — an array
/// index, not a rescan. `u32::MAX` marks a target that fell out of range at load.
fn jump_to(heap: &Heap, proto: RawGc<Proto>, pc: usize) -> Exec<usize> {
    match heap.proto(proto).and_then(|p| p.jump_target(pc)) {
        Some(target) if target != u32::MAX => Ok(target as usize),
        _ => Err(err("jump target out of range")),
    }
}

fn constant(heap: &Heap, proto: RawGc<Proto>, idx: u32) -> Exec<RawValue> {
    // Borrow, don't clone: a table-shaped constant clone allocates, and the
    // hot constant path only ever needs the plain-value arm (a Copy).
    match heap.proto(proto).and_then(|p| p.constant(idx)) {
        Some(RuntimeConstant::Value(v)) => Ok(*v),
        Some(_) => Err(err("constant is not a plain value")),
        None => Err(err("constant index out of range")),
    }
}

fn constant_raw(heap: &Heap, proto: RawGc<Proto>, idx: u32) -> Exec<RuntimeConstant> {
    heap.proto(proto)
        .and_then(|p| p.constant(idx).cloned())
        .ok_or_else(|| err("constant index out of range"))
}

fn child_proto(heap: &Heap, proto: RawGc<Proto>, idx: u32) -> Exec<RawGc<Proto>> {
    heap.proto(proto)
        .and_then(|p| p.child_proto(idx))
        .ok_or_else(|| err("child proto index out of range"))
}

/// The 1-based array key a `GETTABLEN`/`SETTABLEN` `C` operand addresses. It is
/// an integer-valued *number* (the array index), not a native integer — `t[1]`
/// in source is a number key.
fn array_key(c: u8) -> RawValue {
    RawValue::Number(f64::from(c) + 1.0)
}

/// Resolves a `GETIMPORT` path against the thread's globals. The import id packs
/// the component count in the top two bits and up to three 10-bit constant
/// indices (the dotted path components), so `print` is one component and
/// `math.floor` is two. Each component indexes the running value (following
/// `__index`), starting from the global table.
/// Whether the safeenv fast paths may engage: the running closure's
/// environment table is frozen `safeenv` (upstream's `cl->env->safeenv`), so
/// builtin identities cannot have been shadowed. A per-chunk environment or a
/// `setfenv`-swapped table is never safeenv-flagged and correctly disengages.
fn safeenv_active(heap: &Heap, thread: &Thread, closure: RawGc<marker::Closure>) -> bool {
    heap.closure(closure)
        .and_then(|c| c.env)
        .or(thread.globals)
        .and_then(|env| heap.table(env))
        .is_some_and(|env| env.safeenv)
}

/// The FASTCALL1 fast path. Returns the post-CALL pc on success; `None` runs
/// the fallback window. Engages only when: safeenv is active, the argument is
/// a float, the builtin is in the pure-math set, and the fallback CALL is the
/// canonical one-argument/one-result shape.
fn fastcall1(
    heap: &Heap,
    thread: &mut Thread,
    proto: RawGc<Proto>,
    instr: &Instruction,
    pc: usize,
    base: u32,
    closure: RawGc<marker::Closure>,
) -> Option<usize> {
    use ruau_bytecode::opcodes::BuiltinFunction;

    let RawValue::Number(x) = thread.stacks.get(base + u32::from(instr.b)) else {
        return None;
    };
    let result = match instr.a {
        BuiltinFunction::MATH_ABS => x.abs(),
        BuiltinFunction::MATH_CEIL => x.ceil(),
        BuiltinFunction::MATH_FLOOR => x.floor(),
        BuiltinFunction::MATH_SQRT => x.sqrt(),
        _ => return None,
    };
    if !safeenv_active(heap, thread, closure) {
        return None;
    }
    let resident = heap.proto(proto)?;
    let call_pc = resident.jump_target(pc)?;
    if call_pc == u32::MAX {
        return None;
    }
    let call = resident.instruction(call_pc as usize)?;
    // One argument in, exactly one result out, an ordinary CALL: anything
    // else takes the fallback.
    if !matches!(call.opcode, Opcode::Call | Opcode::CallFb) || call.b != 2 || call.c != 2 {
        return None;
    }
    thread
        .stacks
        .set(base + u32::from(call.a), RawValue::Number(result));
    Some(call_pc as usize + 1)
}

fn resolve_import(
    heap: &mut Heap,
    thread: &mut Thread,
    proto: RawGc<Proto>,
    id: u32,
) -> Exec<RawValue> {
    let count = (id >> IMPORT_PATH_COUNT_SHIFT) & 0x3;
    let mut current = active_environment(heap, thread).map_or(RawValue::Nil, RawValue::Table);
    for component in 0..count {
        let index = (id >> import_component_shift(component)) & IMPORT_PATH_COMPONENT_MASK;
        let name = constant(heap, proto, index)?;
        current = index_value(heap, thread, current, name)?;
    }
    Ok(current)
}

pub fn active_environment(heap: &Heap, thread: &Thread) -> Option<RawGc<marker::Table>> {
    let closure = thread
        .call_stack
        .iter()
        .rev()
        .find_map(|entry| entry.frame().map(|frame| frame.closure))?;
    heap.closure(closure)
        .and_then(|closure| closure.env)
        .or(thread.globals)
}

/// Builds a fresh table from a `DUPTABLE` template. A plain key-shape template
/// (the common case) starts empty — the compiler fills it with `SETTABLEKS`; a
/// `TableWithConstants` template carries resolved entries to copy in.
fn make_table(heap: &mut Heap, shape: &TableShape) -> Exec<RawGc<marker::Table>> {
    // Honor the compiler's array-size hint (upstream `NEWTABLE` preallocates):
    // a `{1, 2, 3}` constructor builds its array part once instead of growing
    // through SETLIST.
    let table = if shape.array_hint > 0 {
        let capacity = usize::try_from(shape.array_hint).map_err(|_| err_memory_limit())?;
        let footprint =
            LuaTable::array_capacity_footprint(capacity).ok_or_else(err_memory_limit)?;
        if heap.would_exceed_cap(footprint) {
            return Err(err_memory_limit());
        }
        LuaTable::try_with_array_capacity(capacity)
            .map_err(|_| err_memory("out of memory allocating table array"))?
    } else {
        LuaTable::new()
    };
    let handle = heap
        .alloc_table(table)
        .ok_or_else(|| err_memory("out of memory allocating table"))?;
    if !shape.entries.is_empty() {
        let table = heap
            .table_mut(handle)
            .ok_or_else(|| err("table is not resident"))?;
        for (key, value) in &shape.entries {
            if !matches!(value, RawValue::Nil) {
                table.set(*key, *value);
            }
        }
    }
    Ok(handle)
}

/// `SETLIST`: bulk-store `R[B..]` into the table at `R[A]`, starting at the
/// 1-based array index in the aux word. `C == 0` takes the values up to the live
/// stack top, then restores the top to the active frame's window — the same
/// `L->top = L->ci->top` reset upstream performs after consuming the open-arity
/// values, so a later open-arity op in the frame is not poisoned.
fn set_list(heap: &mut Heap, thread: &mut Thread, base: u32, instr: &Instruction) -> Exec<()> {
    let RawValue::Table(handle) = thread.stacks.get(base + u32::from(instr.a)) else {
        return Err(err("SETLIST target is not a table"));
    };
    // A readonly table rejects the bulk store, like the per-element write paths.
    // The compiler only targets a fresh constructor table, but adversarial
    // validated bytecode could point SETLIST at a frozen library table.
    if heap.table(handle).is_some_and(|t| t.readonly) {
        return Err(err("attempt to modify a readonly table"));
    }
    let first = base + u32::from(instr.b);
    let multret = instr.c == 0;
    let count = if multret {
        thread.top.saturating_sub(first)
    } else {
        u32::from(instr.c) - 1
    };
    let start = aux0(instr)?;
    // SETLIST is one bytecode instruction but does `count` stores; the multret
    // form (`C == 0`) takes `count = top - first`, bounded only by the live
    // register top, so an open-arity spread (`{f()}`, `{...}`) can be large.
    // Charge the whole `count` against the budget upfront so the bulk store costs
    // `O(count)` budget, matching the inline metering on the bulk `table.*`
    // builtins. (The dispatch loop's per-instruction tick covered the SETLIST
    // itself; this adds the per-element cost.)
    if !heap.charge_gas(u64::from(count)) {
        return Err(err_gas());
    }
    for i in 0..count {
        let value = thread.stacks.get(first + i);
        // Array entries key as integer-valued numbers, not native integers.
        let key = RawValue::Number(f64::from(start) + f64::from(i));
        heap.table_mut(handle)
            .ok_or_else(|| err("table is not resident"))?
            .set(key, value);
    }
    if multret {
        let frame_top = thread
            .call_stack
            .iter()
            .rev()
            .find_map(|entry| entry.frame().map(|frame| frame.frame_top))
            .unwrap_or(thread.top);
        thread.top = frame_top;
    }
    Ok(())
}

/// Reads `key` from `value`, following `__index` when the raw entry is absent or
/// the value is not a table: an `__index` function is called with `(value, key)`
/// and a non-function `__index` (typically a table) is re-indexed. The chain is
/// bounded to reject a metatable cycle.
/// The component a single-char `x`/`y`/`z` key selects (case-insensitive), or
/// `None` for any other key — the `(name[0] | ' ') - 'x'` fast path upstream uses
/// for vector field access (`lvmexecute.cpp`).
fn vector_component_index(name: &[u8]) -> Option<usize> {
    match name {
        [c] => {
            let index = (c | 0x20).wrapping_sub(b'x');
            (index < 3).then_some(index as usize)
        }
        _ => None,
    }
}

pub fn index_value(
    heap: &mut Heap,
    thread: &mut Thread,
    value: RawValue,
    key: RawValue,
) -> Exec<RawValue> {
    // Vector field access: `.x`/`.y`/`.z` read the component directly, ahead of the
    // vector metatable's `__index` (which the host installs for named members).
    if let (RawValue::Vector(components), RawValue::String(handle)) = (value, key)
        && let Some(index) = heap
            .string(handle)
            .and_then(|s| vector_component_index(s.bytes()))
    {
        return Ok(RawValue::Number(f64::from(components[index])));
    }
    let mut current = value;
    for _ in 0..heap.limits().max_meta_chain {
        let raw = match current {
            RawValue::Table(handle) => heap.table(handle).map_or(RawValue::Nil, |t| t.get(key)),
            _ => RawValue::Nil,
        };
        if !matches!(raw, RawValue::Nil) {
            return Ok(raw);
        }
        match tm::get_metamethod(heap, current, MetaEvent::Index)? {
            None => {
                return if matches!(current, RawValue::Table(_)) {
                    Ok(RawValue::Nil)
                } else {
                    Err(index_error(heap, current, key))
                };
            }
            Some(handler @ RawValue::Function(_)) => {
                let results = call_value(heap, thread, handler, &[current, key])?;
                return Ok(results.into_iter().next().unwrap_or(RawValue::Nil));
            }
            Some(other) => current = other,
        }
    }
    Err(err("'__index' chain is too long (metatable loop)"))
}

fn namecall_method(
    heap: &mut Heap,
    thread: &mut Thread,
    object: RawValue,
    method_name: RawValue,
) -> Exec<RawValue> {
    let method = index_value(heap, thread, object, method_name)?;
    if matches!(method, RawValue::Nil) {
        return Err(err(format!(
            "attempt to call missing method '{}' of {}",
            key_name(heap, method_name),
            value_type_name(object)
        )));
    }
    Ok(method)
}

fn index_error(heap: &Heap, value: RawValue, key: RawValue) -> crate::call::RaisedError {
    err(format!(
        "attempt to index {} with '{}'",
        value_type_name(value),
        key_name(heap, key)
    ))
}

fn key_name(heap: &Heap, key: RawValue) -> String {
    match key {
        RawValue::String(handle) => heap.string(handle).map_or_else(String::new, |s| {
            String::from_utf8_lossy(s.bytes()).into_owned()
        }),
        other => value_type_name(other).to_owned(),
    }
}

/// Writes `key = value` into `value`, following `__newindex` when the key is
/// absent or the target is not a table: a `__newindex` function is called with
/// `(target, key, value)` and a non-function `__newindex` (typically a table) is
/// re-targeted. An existing key writes raw, with no metamethod.
fn newindex_value(
    heap: &mut Heap,
    thread: &mut Thread,
    target: RawValue,
    key: RawValue,
    value: RawValue,
) -> Exec<()> {
    let mut current = target;
    for _ in 0..heap.limits().max_meta_chain {
        if let RawValue::Table(handle) = current {
            let present = !matches!(
                heap.table(handle).map_or(RawValue::Nil, |t| t.get(key)),
                RawValue::Nil
            );
            if present {
                return raw_table_set(heap, handle, key, value);
            }
            match tm::get_metamethod(heap, current, MetaEvent::NewIndex)? {
                None => return raw_table_set(heap, handle, key, value),
                Some(handler @ RawValue::Function(_)) => {
                    call_value(heap, thread, handler, &[current, key, value])?;
                    return Ok(());
                }
                Some(other) => current = other,
            }
        } else {
            match tm::get_metamethod(heap, current, MetaEvent::NewIndex)? {
                None => return Err(err("attempt to index a non-table value")),
                Some(handler @ RawValue::Function(_)) => {
                    call_value(heap, thread, handler, &[current, key, value])?;
                    return Ok(());
                }
                Some(other) => current = other,
            }
        }
    }
    Err(err("'__newindex' chain is too long (metatable loop)"))
}

/// The raw table write: rejects a readonly table and a `nil`/`NaN` key.
fn raw_table_set(
    heap: &mut Heap,
    handle: RawGc<marker::Table>,
    key: RawValue,
    value: RawValue,
) -> Exec<()> {
    let table = heap
        .table_mut(handle)
        .ok_or_else(|| err("table is not resident"))?;
    if table.readonly {
        return Err(err("attempt to modify a readonly table"));
    }
    if let Some(rejection) = crate::table::key_rejection(key) {
        return Err(err(rejection.message()));
    }
    if !table.set(key, value) {
        return Err(err("table index is invalid"));
    }
    Ok(())
}

/// `#value`: a `__len` metamethod takes priority (a table may override its
/// border); otherwise a table yields its border and a string its byte length.
fn length_of(heap: &mut Heap, thread: &mut Thread, value: RawValue) -> Exec<RawValue> {
    if let Some(handler) = tm::get_metamethod(heap, value, MetaEvent::Len)? {
        let result = call_value(heap, thread, handler, &[value])?
            .into_iter()
            .next()
            .unwrap_or(RawValue::Nil);
        // `luaV_dolen` requires a number; a native integer (a distinct tag) does
        // not qualify.
        return match result {
            RawValue::Number(_) => Ok(result),
            _ => Err(err("'__len' must return a number")),
        };
    }
    let len = match value {
        RawValue::Table(handle) => heap.table(handle).map_or(0, LuaTable::length),
        RawValue::String(handle) => heap.string(handle).map_or(0, |s| s.len() as u64),
        _ => return Err(err("attempt to get length of a non-table value")),
    };
    #[allow(clippy::cast_precision_loss)]
    Ok(RawValue::Number(len as f64))
}

/// `CONCAT`: concatenates registers `first..=last` into one value. Lua concat is
/// right-associative, so it folds from the right, joining adjacent
/// string/number operands and dispatching `__concat` otherwise.
fn concat_range(heap: &mut Heap, thread: &mut Thread, first: u32, last: u32) -> Exec<RawValue> {
    // Fast path: every operand is a string or float — join into one buffer
    // and intern once, instead of interning a pairwise intermediate per
    // operand. Integers (and everything else) keep the metamethod fold.
    if last > first && concat_fast_path(heap, thread, first, last)? {
        return concat_joined(heap, thread, first, last);
    }
    let mut acc = thread.stacks.get(last);
    let mut i = last;
    while i > first {
        i -= 1;
        let lhs = thread.stacks.get(i);
        acc = concat_two(heap, thread, lhs, acc)?;
    }
    Ok(acc)
}

/// Whether `R[first..=last]` are all direct concat operands (string/float).
fn concat_fast_path(heap: &Heap, thread: &Thread, first: u32, last: u32) -> Exec<bool> {
    for i in first..=last {
        match thread.stacks.get(i) {
            RawValue::String(handle) => {
                if heap.string(handle).is_none() {
                    return Err(err("concat operand string is not resident"));
                }
            }
            RawValue::Number(_) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Joins an all-primitive concat range into one interned string: one length
/// pass (rendering floats once), one cap check, one buffer, one intern.
fn concat_joined(heap: &mut Heap, thread: &Thread, first: u32, last: u32) -> Exec<RawValue> {
    let count = (last - first + 1) as usize;
    let mut rendered: Vec<Option<String>> = Vec::with_capacity(count);
    let mut total = 0_usize;
    for i in first..=last {
        match thread.stacks.get(i) {
            RawValue::String(handle) => {
                let len = heap
                    .string(handle)
                    .ok_or_else(|| err("concat operand string is not resident"))?
                    .bytes()
                    .len();
                total = total.saturating_add(len);
                rendered.push(None);
            }
            RawValue::Number(n) => {
                let text = vmutils::number_to_string(n);
                total = total.saturating_add(text.len());
                rendered.push(Some(text));
            }
            _ => return Err(err("concat fast path saw a non-primitive operand")),
        }
    }
    // Same cap enforcement as the pairwise fold: `..` must not build a string
    // past `max_string_bytes` or overshoot the memory cap inside one fold.
    crate::builtins::meter_string_growth(heap, total, "string concatenation")?;
    let mut bytes = Vec::with_capacity(total);
    for (offset, slot) in rendered.into_iter().enumerate() {
        match slot {
            Some(text) => bytes.extend_from_slice(text.as_bytes()),
            None => {
                let RawValue::String(handle) = thread.stacks.get(first + offset as u32) else {
                    return Err(err("concat fast path operand changed shape"));
                };
                let string = heap
                    .string(handle)
                    .ok_or_else(|| err("concat operand string is not resident"))?;
                bytes.extend_from_slice(string.bytes());
            }
        }
    }
    let interned = heap
        .intern_str(&bytes)
        .ok_or_else(|| err_memory("out of memory interning a concatenated string"))?;
    Ok(RawValue::String(interned))
}

/// Concatenates two values: strings and numbers join directly into a new
/// interned string; anything else (including this revision's integers) dispatches
/// `__concat` from the left operand, then the right.
fn concat_two(
    heap: &mut Heap,
    thread: &mut Thread,
    lhs: RawValue,
    rhs: RawValue,
) -> Exec<RawValue> {
    if let (Some(lhs_src), Some(rhs_src)) = (concat_source(lhs), concat_source(rhs)) {
        let total = lhs_src.bytes(heap).len() + rhs_src.bytes(heap).len();
        // `CONCAT` folds every operand in one bytecode instruction, interning each
        // intermediate, so it must enforce the per-string size cap and the memory cap
        // here (not just at the post-instruction safepoint) or `..` would be a way to
        // build a string past `max_string_bytes` that the dedicated builders reject and
        // to overshoot the memory cap within the fold. Reuse the builders' check.
        crate::builtins::meter_string_growth(heap, total, "string concatenation")?;
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(lhs_src.bytes(heap));
        bytes.extend_from_slice(rhs_src.bytes(heap));
        let interned = heap
            .intern_str(&bytes)
            .ok_or_else(|| err_memory("out of memory interning a concatenated string"))?;
        return Ok(RawValue::String(interned));
    }
    let handler = match tm::get_metamethod(heap, lhs, MetaEvent::Concat)? {
        Some(handler) => handler,
        None => tm::get_metamethod(heap, rhs, MetaEvent::Concat)?
            .ok_or_else(|| err(concat_type_error(lhs, rhs)))?,
    };
    let results = call_value(heap, thread, handler, &[lhs, rhs])?;
    Ok(results.into_iter().next().unwrap_or(RawValue::Nil))
}

/// `luaG_concaterror`'s "attempt to concatenate %s with %s", naming both operand
/// types in order — reached only when neither operand is a string/number nor has
/// a `__concat` (e.g. `"1" .. nil` is "concatenate string with nil").
fn concat_type_error(lhs: RawValue, rhs: RawValue) -> String {
    let name = |value| core::str::from_utf8(crate::builtins::type_name(value)).unwrap_or("value");
    format!("attempt to concatenate {} with {}", name(lhs), name(rhs))
}

/// The concat source of a string (its interned handle) or a number (its
/// rendering); `None` for any other value — including this revision's
/// integers — which routes to `__concat`.
fn concat_source(value: RawValue) -> Option<crate::builtins::StrArg> {
    match value {
        RawValue::String(handle) => Some(crate::builtins::StrArg::Interned(handle)),
        RawValue::Number(n) => Some(crate::builtins::StrArg::Coerced(
            vmutils::number_to_string(n).into_bytes(),
        )),
        _ => None,
    }
}

#[cfg(any())]
mod tests;
