//! Call frames, the protected-call core, and the engine error model (port
//! `ldo.cpp`).
//!
//! A Lua call is iterative: [`precall`] pushes a [`CallInfo`] and the dispatch
//! loop continues in the callee; [`return_op`] pops it. A deep Lua chain never
//! recurses in Rust.
//!
//! [`protected`] is the `luaD_pcall` analog: it records the frame depth and the
//! live stack top, runs a region to completion, and on an unwind closes any
//! open upvalues over the abandoned frames and restores the depth and top. The
//! top-level [`run`] is `protected` at depth zero. Bytecode `pcall` is modeled
//! as explicit thread state so it can yield with the ordinary Lua stack intact.

use ruau_bytecode::{Instruction, opcodes::Opcode};
use ruau_vm_api::{
    HostCall, HostFuture, HostPayload, HostUnwind, OwnedValue, RawGc, RawValue, RegistryRef,
    ScriptErrorField, Unwind, marker,
};

use crate::{
    builtins,
    execute::{DispatchMode, close_upvals_from, dispatch},
    heap::Heap,
    host::{EngineContext, HostCallable},
    object::{HostId, Proto},
    scope,
    state::{
        CallInfo, CallStackEntry, CallStackReserveError, CapturedVarargs,
        ConformanceNativeContinuation, ProtectedInfo, RequireInfo, ResumeSlot, Step, SuspendedCall,
        SuspendedRequire, SuspendedTarget, Thread,
    },
    table::LuaTable,
    tm::{self, MetaEvent},
};

/// The "attempt to call a `<type>` value" message Luau raises for a non-callable value
/// (`luaG_callerror`), naming the value's type — `nil`, `number`, `table`, … — rather than a
/// generic "non-function".
fn call_type_error(value: RawValue) -> String {
    let type_name = core::str::from_utf8(builtins::type_name(value)).unwrap_or("value");
    format!("attempt to call a {type_name} value")
}

/// What a [`RaisedError`] carries until it surfaces as a Lua error object.
#[derive(Debug)]
pub enum ErrorPayload {
    /// An engine message, interned to a Lua string at the boundary and given a
    /// `source:line:` prefix unless already located.
    Message(String),
    /// A host-authored structured error table, materialized at the same
    /// boundary as message strings. The canonical message is written as the
    /// table's `message` field before host-supplied fields are applied.
    Structured {
        message: String,
        fields: Vec<ScriptErrorField>,
    },
    /// A script-thrown error value (`error(v)` for a non-string-coercible `v`),
    /// surfaced unchanged — never prefixed.
    Value(RawValue),
}

pub use ruau_vm_api::RuntimeErrorKind;

/// A runtime error in flight. `located` records whether a `source:line:` prefix
/// has been attached to a [`Message`](ErrorPayload::Message); the enclosing
/// protected boundary prefixes an unlocated message with the requested stack
/// frame's location (`luaG_runerror`). A value payload, and `error(v, 0)`, carry
/// no location.
#[derive(Debug)]
pub struct RaisedError {
    /// The error object, as a deferred message or a concrete value.
    pub payload: ErrorPayload,
    /// Whether a location prefix has been attached (or is not wanted).
    pub located: bool,
    /// One-based frame level used when attaching a location. Level 1 is the
    /// innermost script frame; higher levels walk outward.
    pub location_level: usize,
    /// The error category, deciding catchability and the request metric.
    pub kind: RuntimeErrorKind,
    /// Typed host freight attached by the raising host function
    /// (`RuntimeError::with_payload`). It rides the in-flight error untouched;
    /// when the error is materialized into a Lua value (a `pcall` catch or an
    /// exit boundary), [`materialize`] hands it to the heap's payload tracker so
    /// a script re-raise of the same error value carries it back out. Engine
    /// errors never set this.
    pub host_payload: Option<HostPayload>,
}

impl RaisedError {
    /// Whether `pcall`/`xpcall` may catch this error.
    #[must_use]
    pub fn is_catchable(&self) -> bool {
        self.kind.catchable()
    }
}

/// Builds a [`RaisedError`] the protected boundary will prefix with the failing
/// frame's `source:line:` location.
pub fn err(message: impl Into<String>) -> RaisedError {
    err_at_level(message, 1)
}

/// Builds a [`RaisedError`] that reports the requested one-based caller frame.
pub fn err_at_level(message: impl Into<String>, level: usize) -> RaisedError {
    RaisedError {
        payload: ErrorPayload::Message(message.into()),
        located: false,
        location_level: level.max(1),
        kind: RuntimeErrorKind::Runtime,
        host_payload: None,
    }
}

/// Builds a [`RaisedError`] with a specific catchable failure kind. Located
/// like [`err`].
pub fn err_kind(message: impl Into<String>, kind: RuntimeErrorKind) -> RaisedError {
    RaisedError {
        payload: ErrorPayload::Message(message.into()),
        located: false,
        location_level: 1,
        kind,
        host_payload: None,
    }
}

/// Builds a string [`RaisedError`] that must not receive a location prefix.
pub fn err_no_location(message: impl Into<String>) -> RaisedError {
    RaisedError {
        payload: ErrorPayload::Message(message.into()),
        located: true,
        location_level: 1,
        kind: RuntimeErrorKind::Runtime,
        host_payload: None,
    }
}

/// Builds a [`RaisedError`] from a thrown error value, surfaced unchanged.
pub fn err_value(value: RawValue) -> RaisedError {
    RaisedError {
        payload: ErrorPayload::Value(value),
        located: true,
        location_level: 1,
        kind: RuntimeErrorKind::Runtime,
        host_payload: None,
    }
}

/// Builds the fatal cancellation error. It carries no location prefix and is
/// uncatchable: it propagates past `pcall`/`xpcall` so a tenant cannot swallow a
/// cancellation and continue running.
pub fn err_cancelled() -> RaisedError {
    RaisedError {
        payload: ErrorPayload::Message("cancelled".to_string()),
        located: true,
        location_level: 1,
        kind: RuntimeErrorKind::Cancelled,
        host_payload: None,
    }
}

/// Builds a fatal deadline error. Like [`err_cancelled`], it is uncatchable.
pub fn err_deadline(message: impl Into<String>) -> RaisedError {
    RaisedError {
        payload: ErrorPayload::Message(message.into()),
        located: true,
        location_level: 1,
        kind: RuntimeErrorKind::Deadline,
        host_payload: None,
    }
}

/// Builds a memory-cap or allocation-failure error. It is catchable (like Lua),
/// but carries the `Memory` category so a runner can report it as a memory
/// failure rather than an ordinary runtime error. Located like [`err`].
pub fn err_memory(message: impl Into<String>) -> RaisedError {
    RaisedError {
        payload: ErrorPayload::Message(message.into()),
        located: false,
        location_level: 1,
        kind: RuntimeErrorKind::Memory,
        host_payload: None,
    }
}

/// The gas-exhaustion error every metered builtin and dispatch site raises;
/// tests assert on this exact text.
pub fn err_gas() -> RaisedError {
    err("instruction budget exhausted")
}

/// The memory-cap error raised when an allocation would cross the tenant cap;
/// tests assert on this exact text.
pub fn err_memory_limit() -> RaisedError {
    err_memory("memory limit exceeded")
}

/// The register-stack reservation failure shared by every call path.
pub fn err_register_stack_oom() -> RaisedError {
    err_memory("not enough memory for the register stack")
}

fn call_stack_reserve_error(error: &CallStackReserveError) -> RaisedError {
    match error {
        CallStackReserveError::Depth => err("stack overflow"),
        CallStackReserveError::Alloc => err_memory("not enough memory for the call stack"),
    }
}

pub fn reserve_call_entries(heap: &Heap, thread: &mut Thread, additional: usize) -> Exec<()> {
    thread
        .reserve_call_stack_entries(heap.limits().max_call_depth, additional)
        .map_err(|error| call_stack_reserve_error(&error))
}

pub fn push_call_entry(heap: &Heap, thread: &mut Thread, entry: CallStackEntry) -> Exec<()> {
    thread
        .push_call_stack_entry(heap.limits().max_call_depth, entry)
        .map_err(|error| call_stack_reserve_error(&error))
}

pub fn empty_varargs(heap: &Heap) -> CapturedVarargs {
    CapturedVarargs::new(heap.meter())
}

pub fn capture_varargs_from_slice(
    heap: &Heap,
    args: &[RawValue],
    start: usize,
) -> Exec<CapturedVarargs> {
    if start >= args.len() {
        return Ok(empty_varargs(heap));
    }
    let count = args.len() - start;
    if count > heap.limits().max_varargs {
        return Err(err("too many arguments to a variadic function"));
    }
    let mut varargs = CapturedVarargs::with_capacity(heap.meter(), count)
        .map_err(|_| err_memory("not enough memory for captured varargs"))?;
    for &value in &args[start..] {
        varargs.push_reserved(value);
    }
    Ok(varargs)
}

pub fn ensure_result_values(heap: &Heap, count: usize, context: &str) -> Exec<()> {
    if count > heap.limits().max_table_elements {
        return Err(err(format!("too many {context} results")));
    }
    if heap.would_exceed_cap(count.saturating_mul(std::mem::size_of::<RawValue>())) {
        return Err(err_memory_limit());
    }
    Ok(())
}

pub fn prepare_result_copy(heap: &mut Heap, count: usize, context: &str) -> Exec<()> {
    ensure_result_values(heap, count, context)?;
    if !heap.charge_gas(u64::try_from(count).unwrap_or(u64::MAX)) {
        return Err(err_gas());
    }
    Ok(())
}

pub fn collect_stack_results(
    heap: &mut Heap,
    thread: &Thread,
    start: u32,
    count: u32,
    context: &str,
) -> Exec<Vec<RawValue>> {
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    prepare_result_copy(heap, count, context)?;
    let mut results = Vec::new();
    results
        .try_reserve(count)
        .map_err(|_| err_memory("not enough memory for result values"))?;
    for offset in 0..count {
        results.push(
            thread
                .stacks
                .get(start.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX))),
        );
    }
    Ok(results)
}

fn capture_varargs_from_stack(
    heap: &Heap,
    thread: &Thread,
    start: u32,
    end: u32,
) -> Exec<CapturedVarargs> {
    if start >= end {
        return Ok(empty_varargs(heap));
    }
    let count = (end - start) as usize;
    if count > heap.limits().max_varargs {
        return Err(err("too many arguments to a variadic function"));
    }
    let mut varargs = CapturedVarargs::with_capacity(heap.meter(), count)
        .map_err(|_| err_memory("not enough memory for captured varargs"))?;
    for i in start..end {
        varargs.push_reserved(thread.stacks.get(i));
    }
    Ok(varargs)
}

/// Builds the fixed `xpcall` handler-failure error. Lua surfaces this as the
/// string `"error in error handling"`, but the category remains distinct for
/// host metrics when the error is not converted into `xpcall` results.
pub fn err_handler_failure() -> RaisedError {
    RaisedError {
        payload: ErrorPayload::Message("error in error handling".to_string()),
        located: true,
        location_level: 1,
        kind: RuntimeErrorKind::HandlerFailure,
        host_payload: None,
    }
}

/// Converts a host-supplied [`HostUnwind`] (from a synchronous `HostCall::Ready(Err)`)
/// to a [`RaisedError`], **preserving its category** so a host that raises a fatal
/// (cancellation/deadline) error is not silently downgraded to a catchable one. The
/// owned error value is materialized through the token-checked registry, so a forged
/// `Pinned` ref in the *error* object is rejected just like one in a return value —
/// that rejection becomes the surfaced error.
///
/// The raw `HostFunction` ABI carries no typed payload: `HostUnwind` is owned
/// data only, so payload-carrying errors are a scoped/async host-function
/// feature ([`scope::RuntimeError::with_payload`]).
fn host_unwind_to_error(heap: &mut Heap, unwind: HostUnwind) -> RaisedError {
    let HostUnwind {
        error,
        kind,
        script_fields,
    } = unwind;
    if !script_fields.is_empty() {
        release_owned_pins(heap, std::slice::from_ref(&error));
        return RaisedError {
            payload: ErrorPayload::Structured {
                message: owned_error_message(&error),
                fields: script_fields,
            },
            located: true,
            location_level: 1,
            kind,
            host_payload: None,
        };
    }
    match materialize_owned(heap, &error) {
        Ok(error) => RaisedError {
            payload: ErrorPayload::Value(error),
            located: true,
            location_level: 1,
            kind,
            host_payload: None,
        },
        Err(rejection) => rejection,
    }
}

fn scoped_host_error_to_runtime(error: scope::RuntimeError) -> RaisedError {
    let (message, kind, host_payload, script_fields) = error.into_error_parts();
    RaisedError {
        payload: error_payload_from_message(message, script_fields),
        located: false,
        location_level: 1,
        kind,
        host_payload,
    }
}

pub fn error_payload_from_message(message: String, fields: Vec<ScriptErrorField>) -> ErrorPayload {
    if fields.is_empty() {
        ErrorPayload::Message(message)
    } else {
        ErrorPayload::Structured { message, fields }
    }
}

fn owned_error_message(error: &OwnedValue) -> String {
    match error {
        OwnedValue::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => "host error".to_string(),
    }
}

/// Resolves an error to the Lua value it surfaces as: an interned string for a
/// message, or the thrown value verbatim.
///
/// A typed host payload riding the error is handed to the heap's
/// [`HostPayloadTracker`] keyed by the materialized value, so a later script
/// re-raise of the same value (`error(caught, 0)`) carries it back out to a
/// host exit surface. This runs only when an error becomes a Lua value — a
/// `pcall` catch or an unwind boundary — never on a non-error path.
pub fn materialize(heap: &mut Heap, error: RaisedError) -> RawValue {
    let value = match error.payload {
        ErrorPayload::Message(message) => heap
            .intern_str(message.as_bytes())
            .map_or(RawValue::Nil, RawValue::String),
        ErrorPayload::Structured { message, fields } => {
            materialize_structured_error(heap, &message, &fields)
        }
        ErrorPayload::Value(value) => value,
    };
    if let Some(payload) = error.host_payload {
        heap.host_error_payloads.track(value, payload);
    }
    value
}

fn materialize_structured_error(
    heap: &mut Heap,
    message: &str,
    fields: &[ScriptErrorField],
) -> RawValue {
    let result = (|| {
        let table = heap
            .alloc_table(LuaTable::new())
            .ok_or_else(|| err_memory("out of memory allocating structured host error"))?;
        let message = heap
            .intern_str(message.as_bytes())
            .map(RawValue::String)
            .ok_or_else(|| err_memory("out of memory interning structured host error message"))?;
        set_structured_error_field(heap, table, "message", message)?;
        for field in fields {
            let value = materialize_owned(heap, &field.value)?;
            set_structured_error_field(heap, table, field.name.as_ref(), value)?;
        }
        Ok(RawValue::Table(table))
    })();
    release_script_error_field_pins(heap, fields);
    match result {
        Ok(value) => value,
        Err(error) => materialize(heap, error),
    }
}

fn set_structured_error_field(
    heap: &mut Heap,
    table: RawGc<marker::Table>,
    name: &str,
    value: RawValue,
) -> Exec<()> {
    let key = heap
        .intern_str(name.as_bytes())
        .map(RawValue::String)
        .ok_or_else(|| err_memory("out of memory interning structured host error field"))?;
    let table = heap
        .table_mut(table)
        .ok_or_else(|| err("structured host error table disappeared"))?;
    if table.set(key, value) {
        Ok(())
    } else {
        Err(err("structured host error field key is invalid"))
    }
}

fn release_script_error_field_pins(heap: &mut Heap, fields: &[ScriptErrorField]) {
    for field in fields {
        if let OwnedValue::Pinned(reference) = &field.value {
            heap.unpin(reference);
        }
    }
}

/// Recovers the typed host payload of a failure at a host exit surface: the
/// payload riding the in-flight [`RaisedError`] directly (a host raise that no
/// script caught), or the tracker entry for the materialized error value (a
/// script `pcall` caught the host error and re-raised the same value). Call
/// with the payload taken *before* [`materialize`] consumed the error, and the
/// value it produced.
pub fn recover_host_payload(
    heap: &Heap,
    in_flight: Option<HostPayload>,
    value: RawValue,
) -> Option<HostPayload> {
    in_flight.or_else(|| heap.host_error_payloads.get(value))
}

/// The error-value identity a tracked payload is keyed by: the variant tag plus
/// the generational arena handle. Generations make a stale key inert — a
/// collected error value's slot is re-stamped on reuse, so a dead entry can
/// never match a newer value — which is why tracked values need no registry
/// rooting. Immediates (nil, booleans, numbers, vectors) have no identity and
/// are not trackable. Host-raised errors materialize as strings by default, or
/// as tables when a host opts into script-visible structured fields; both have
/// stable heap identity while the script keeps them live.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PayloadKey {
    tag: u8,
    index: u32,
    generation: u32,
}

fn payload_key(value: RawValue) -> Option<PayloadKey> {
    fn key<T>(tag: u8, handle: RawGc<T>) -> Option<PayloadKey> {
        Some(PayloadKey {
            tag,
            index: handle.index(),
            generation: handle.generation(),
        })
    }
    match value {
        RawValue::String(handle) => key(0, handle),
        RawValue::Table(handle) => key(1, handle),
        RawValue::Function(handle) => key(2, handle),
        RawValue::Userdata(handle) => key(3, handle),
        RawValue::Thread(handle) => key(4, handle),
        RawValue::Buffer(handle) => key(5, handle),
        RawValue::Nil
        | RawValue::Boolean(_)
        | RawValue::Number(_)
        | RawValue::Integer(_)
        | RawValue::Vector(_)
        | RawValue::LightUserdata { .. } => None,
    }
}

/// The most recent payload-carrying error values, keyed by value identity.
///
/// This is the side table that lets a typed host payload survive a script
/// `pcall` round trip: when a payload-carrying error is materialized into a Lua
/// value (the `pcall` catch), the payload is parked here under the value's
/// identity; when the script re-raises that same value and it reaches a host
/// exit surface, the surface recovers the payload by looking the value up
/// again. Inner `pcall`/`xpcall` hops in between cost nothing — the value keeps
/// its identity, so the entry simply stays valid.
///
/// Loss semantics follow value identity. Strings are interned, so identity is
/// content equality: `error(caught, 0)` re-raises the same string and preserves
/// the payload, while `error(caught)` (which prefixes a new `source:line:`),
/// `tostring`, concatenation, or raising any other value produce a *different*
/// value and drop it. A payload also cannot be smuggled onto an unrelated
/// error: tracking happens only at the engine's own materialization boundary.
///
/// The table is a fixed-capacity FIFO ring ([`MAX_TRACKED_PAYLOADS`]) of
/// `Arc`-shared payloads, so it cannot grow with adversarial catch-and-discard
/// traffic; the oldest entry is evicted first. Entries hold no GC root — key
/// generations make stale entries inert (see [`PayloadKey`]) — and are dropped
/// on eviction or VM teardown.
#[derive(Default)]
pub struct HostPayloadTracker {
    entries: Vec<(PayloadKey, HostPayload)>,
}

/// Tracked-payload capacity. Re-raise round trips resolve within a handful of
/// in-flight errors; the cap only bounds pathological catch-and-discard loops.
const MAX_TRACKED_PAYLOADS: usize = 64;

impl HostPayloadTracker {
    /// Parks `payload` under `value`'s identity, replacing an existing entry
    /// for the same value and evicting the oldest entry at capacity. A value
    /// with no identity (an immediate) is not tracked.
    fn track(&mut self, value: RawValue, payload: HostPayload) {
        let Some(key) = payload_key(value) else {
            return;
        };
        self.entries.retain(|(existing, _)| *existing != key);
        if self.entries.len() >= MAX_TRACKED_PAYLOADS {
            self.entries.remove(0);
        }
        self.entries.push((key, payload));
    }

    /// The payload parked under `value`'s identity, if any.
    fn get(&self, value: RawValue) -> Option<HostPayload> {
        let key = payload_key(value)?;
        self.entries
            .iter()
            .find(|(existing, _)| *existing == key)
            .map(|(_, payload)| payload.clone())
    }
}

/// The result of an engine operation that may raise.
pub type Exec<T> = Result<T, RaisedError>;

/// A protected-call failure, with optional pre-unwind traceback.
pub struct ProtectedFailure {
    /// The located error to materialize or propagate.
    pub error: RaisedError,
    /// The rendered text of the stack traceback captured before the protected
    /// boundary unwound the failing frames. The capture's structured frames are
    /// stashed on the failing thread (`Thread::captured_traceback`): every
    /// protected surface consumes this text, and the embedder error surface
    /// additionally attaches frames by re-pairing the stash with this failure
    /// (matching the stash's rendered text against this one).
    pub traceback: Option<String>,
}

enum DispatchedHostCall {
    Raw {
        call: HostCall,
        pins: Vec<RegistryRef>,
    },
    Scoped(Exec<Vec<RawValue>>),
    AsyncScoped {
        future: HostFuture,
        host_requests: crate::host::HostRequests,
    },
}

fn release_host_pins(heap: &mut Heap, pins: Vec<RegistryRef>) {
    for reference in pins {
        heap.unpin(&reference);
    }
}

pub fn release_owned_pins(heap: &mut Heap, values: &[OwnedValue]) {
    for value in values {
        if let OwnedValue::Pinned(reference) = value {
            heap.unpin(reference);
        }
    }
}

/// Materializes the first `want` of a synchronous host return's owned values into
/// rooted `RawValue`s, the same way the async driver materializes a `HostReturn`:
/// scalars directly, bytes interned through the accounted heap, and `Pinned` refs
/// resolved (and consumed) through the token-checked registry. Because the return
/// carries only [`OwnedValue`], a raw forged handle is unrepresentable; a forged
/// `Pinned` ref is rejected here by the registry's provenance check.
///
/// Only the `want` *observed* values are materialized: like the async
/// `materialize_return`, a fixed-arity `CALL` that ignores a tail must not intern
/// or reject (or fail on a stale `Pinned` in) a value it never reads. Pass
/// `values.len()` for the multret / protected paths that observe everything.
fn materialize_sync_results(
    heap: &mut Heap,
    values: &[OwnedValue],
    want: usize,
) -> Exec<Vec<RawValue>> {
    let result = (|| {
        prepare_result_copy(heap, want, "host-return")?;
        values
            .iter()
            .take(want)
            .map(|value| materialize_owned(heap, value))
            .collect()
    })();
    release_owned_pins(heap, values);
    result
}

/// Materializes one [`OwnedValue`] into a rooted `RawValue`: a scalar directly,
/// owned bytes interned through the accounted heap, or a registry pin resolved
/// (and consumed) through the token-checked registry. Shared by the async driver
/// and the synchronous host-result boundary, which both take owned returns.
pub fn materialize_owned(heap: &mut Heap, value: &OwnedValue) -> Exec<RawValue> {
    Ok(match value {
        OwnedValue::Nil => RawValue::Nil,
        OwnedValue::Boolean(b) => RawValue::Boolean(*b),
        OwnedValue::Number(n) => RawValue::Number(*n),
        OwnedValue::Integer(i) => RawValue::Integer(*i),
        OwnedValue::Vector(v) => RawValue::Vector(*v),
        OwnedValue::LightUserdata { handle, tag } => RawValue::LightUserdata {
            handle: *handle,
            tag: *tag,
        },
        OwnedValue::Bytes(bytes) => heap
            .intern_str(bytes)
            .map(RawValue::String)
            .ok_or_else(|| err_memory("out of memory interning an async host result"))?,
        OwnedValue::Pinned(reference) => {
            let value = heap.pinned_value(reference).map_err(err)?;
            heap.unpin(reference);
            value
        }
    })
}

/// The number of host results a `CALL` observes: all of them for an open-arity
/// (`C == 0`, multret) call, else its fixed `C - 1` capped at what the host
/// actually produced — a call site cannot observe more results than exist, and
/// padding the shortfall with nil is `place_results`' job. Mirrors the async
/// `materialize_return` arity rule.
fn observed_results(result_count: u8, available: usize) -> usize {
    if result_count == 0 {
        available
    } else {
        available.min(usize::from(result_count) - 1)
    }
}

/// Runs `main` with no arguments to completion, returning its results. The
/// public synchronous entry point; the async driver uses the same root setup.
///
/// # Errors
/// Returns the [`Unwind`] of an uncaught runtime error, its message interned as
/// a Lua string.
pub fn run(
    heap: &mut Heap,
    thread: &mut Thread,
    main: RawGc<marker::Closure>,
) -> Result<Vec<RawValue>, Unwind> {
    match run_protected(heap, thread, main) {
        Ok(results) => Ok(results),
        Err(error) => {
            let kind = error.kind;
            Err(Unwind {
                error: materialize(heap, error),
                kind,
            })
        }
    }
}

/// Builds the root frame for `main` and runs it under [`protected`].
fn run_protected(
    heap: &mut Heap,
    thread: &mut Thread,
    main: RawGc<marker::Closure>,
) -> Exec<Vec<RawValue>> {
    let frame = root_frame(heap, thread, main)?;
    match protected(heap, thread, frame, None)? {
        Ok(results) => Ok(results),
        Err(failure) => Err(failure.error),
    }
}

/// Builds the root frame for `main` and runs it under [`protected`], returning
/// catchable script failures separately from root-frame setup failures.
pub fn run_protected_with_traceback(
    heap: &mut Heap,
    thread: &mut Thread,
    main: RawGc<marker::Closure>,
    max_traceback_bytes: usize,
) -> Result<Result<Vec<RawValue>, ProtectedFailure>, RaisedError> {
    let frame = root_frame(heap, thread, main)?;
    protected(heap, thread, frame, Some(max_traceback_bytes))
}

/// Builds the root `CallInfo` for a no-argument call to `main`, sizing the
/// thread's register stack to the prototype's window. Shared by the synchronous
/// [`run_protected`] and the async driver's root setup.
pub fn root_frame(
    heap: &Heap,
    thread: &mut Thread,
    main: RawGc<marker::Closure>,
) -> Exec<CallInfo> {
    let proto = closure_proto(heap, main)?;
    let frame_top = heap
        .proto(proto)
        .map_or(0, |p| u32::from(p.max_stack_size))
        .max(1);
    thread
        .stacks
        .ensure(frame_top)
        .map_err(|_| err_register_stack_oom())?;
    Ok(CallInfo {
        closure: main,
        proto,
        base: 0,
        result_base: 0,
        frame_top,
        savedpc: 0,
        nresults: -1,
        // The main chunk runs with no script arguments, so its `...` is empty.
        varargs: empty_varargs(heap),
    })
}

/// Builds the root [`CallInfo`] for an embedder-owned Lua closure call with raw
/// arguments. This is the suspendable counterpart to [`run_function`]'s
/// non-yieldable nested call path: the returned frame is driven by the async
/// root dispatcher, so direct async host calls may suspend.
///
/// Raw handles are checked before they enter the root frame. The call target must
/// be a Luau closure; native/builtin/host direct targets remain the synchronous
/// [`call_value`] path until the driver grows a synthetic root `CALL` site.
pub fn root_function_frame(
    heap: &Heap,
    thread: &mut Thread,
    func: RawValue,
    args: &[RawValue],
) -> Exec<CallInfo> {
    if let Err(message) = validate_call_inputs(heap, func, args) {
        return Err(err(message));
    }
    let RawValue::Function(closure) = func else {
        return Err(err(call_type_error(func)));
    };
    let proto = closure_proto(heap, closure)?;
    if heap
        .proto(proto)
        .is_some_and(|proto| proto.native.is_some() || proto.host.is_some())
    {
        return Err(err("protected async function call requires a Luau closure"));
    }
    let (num_params, is_vararg, max_stack) = heap
        .proto(proto)
        .map(|p| {
            (
                u32::from(p.num_params),
                p.is_vararg,
                u32::from(p.max_stack_size).max(1),
            )
        })
        .ok_or_else(|| err("callee has no prototype"))?;
    let nargs = u32::try_from(args.len()).map_err(|_| err("too many call arguments"))?;
    let frame_top = max_stack.max(num_params).max(1);
    thread
        .stacks
        .ensure(frame_top)
        .map_err(|_| err_register_stack_oom())?;
    let fixed_args = nargs.min(num_params);
    for i in 0..fixed_args {
        thread.stacks.set(
            i,
            args[usize::try_from(i).expect("argument index fits usize")],
        );
    }
    for i in fixed_args..num_params {
        thread.stacks.set(i, RawValue::Nil);
    }
    let varargs = if is_vararg && nargs > num_params {
        capture_varargs_from_slice(heap, args, num_params as usize)?
    } else {
        empty_varargs(heap)
    };
    Ok(CallInfo {
        closure,
        proto,
        base: 0,
        result_base: 0,
        frame_top,
        savedpc: 0,
        nresults: -1,
        varargs,
    })
}

/// Runs `frame` and the calls it makes to completion in a fresh protected scope.
/// On an unwind, closes open upvalues over the abandoned frames and restores the
/// thread's frame depth and stack top before propagating — the `luaD_pcall`
/// core (`ldo.cpp:729-800`).
fn protected(
    heap: &mut Heap,
    thread: &mut Thread,
    frame: CallInfo,
    traceback_limit: Option<usize>,
) -> Result<Result<Vec<RawValue>, ProtectedFailure>, RaisedError> {
    let floor = thread.call_stack.len();
    let saved_top = thread.top;
    let frame_top = frame.frame_top;
    push_call_entry(heap, thread, CallStackEntry::Frame(frame))?;
    thread.top = frame_top;

    match dispatch(heap, thread, floor, DispatchMode::RootSync) {
        Ok(Step::Return(results)) => Ok(Ok(results)),
        // A return is the only non-error outcome of this *synchronous* protected
        // region. A yield that reaches here crossed the main thread or a pcall
        // boundary; a suspend means the script awaited an async host call under
        // the synchronous entry, which must instead use the async driver. Attach
        // the failing frame's location while the call stack is intact, then unwind.
        other => {
            let error = match other {
                Err(error) => error,
                Ok(Step::Suspend(_)) => err(
                    "async host call reached through the synchronous entry; use the async driver",
                ),
                Ok(Step::SuspendRequire(require)) => {
                    builtins::release_suspended_require(heap, require);
                    err(builtins::ASYNC_REQUIRE_SYNC_ENTRY_ERROR)
                }
                _ => err("attempt to yield across a protected-call boundary"),
            };
            Ok(Err(unwind_protected_failure(
                heap,
                thread,
                floor,
                saved_top,
                error,
                traceback_limit,
            )))
        }
    }
}

/// Unwinds a protected boundary and optionally captures the failing stack first.
pub fn unwind_protected_failure(
    heap: &mut Heap,
    thread: &mut Thread,
    floor: usize,
    saved_top: u32,
    error: RaisedError,
    traceback_limit: Option<usize>,
) -> ProtectedFailure {
    let error = crate::debug::locate(heap, thread, error);
    let capture =
        traceback_limit.and_then(|max_bytes| crate::debug::traceback(heap, thread, max_bytes));
    let traceback = capture.as_ref().map(|capture| capture.text().to_owned());
    // The failure carries the rendered text; the structured frames ride the
    // thread so the embedder surface can re-pair them with this failure.
    // Assigned unconditionally: an unwind that captures nothing clears any
    // stale stash from an earlier protected boundary.
    thread.captured_traceback = capture;
    close_upvalues(heap, thread, floor);
    truncate_call_stack(heap, thread, floor);
    thread.top = saved_top;
    ProtectedFailure { error, traceback }
}

/// Closes open upvalues whose stack slot sits in or above the abandoned frames
/// (`luaF_close` on unwind), so a captured local that outlives the failed call
/// keeps its last value.
fn close_upvalues(heap: &mut Heap, thread: &mut Thread, floor: usize) {
    if let Some(base) = first_frame_base_at_or_after(thread, floor) {
        close_upvals_from(heap, thread, base);
    }
}

fn first_frame_base_at_or_after(thread: &Thread, floor: usize) -> Option<u32> {
    thread
        .call_stack
        .iter()
        .skip(floor)
        .find_map(|entry| entry.frame().map(|frame| frame.base))
}

fn truncate_call_stack(heap: &mut Heap, thread: &mut Thread, floor: usize) {
    for entry in thread.call_stack.drain(floor..) {
        cleanup_call_stack_entry(heap, entry);
    }
}

fn cleanup_call_stack_entry(heap: &mut Heap, entry: CallStackEntry) {
    if let CallStackEntry::Require(require) = entry {
        heap.module_load_end(&require.loading_key);
        heap.unpin(&require.module_pin);
    }
}

/// Resolves the prototype a closure runs.
pub fn closure_proto(heap: &Heap, closure: RawGc<marker::Closure>) -> Exec<RawGc<Proto>> {
    heap.closure(closure)
        .map(|c| c.proto)
        .ok_or_else(|| err("called value is not a live closure"))
}

/// Calls a Lua closure `func` with `args` from native code — a metamethod
/// or a builtin — running it to completion in a nested dispatch and returning its
/// results. This is the native-recursion model: a metamethod nests a
/// fresh `dispatch`, bounded by `max_native_depth`. Host functions that try to
/// suspend across this boundary return an error; suspension is supported only by
/// the async driver at the root dispatch.
///
/// The call and its arguments are placed above the active frame's window, so the
/// caller's live registers are untouched. On an error, the frames stay on the
/// stack and unwind at the next enclosing protected boundary.
///
/// # Errors
/// Returns the error of a non-callable value, a memory failure, or anything the
/// callee raises.
pub fn call_value(
    heap: &mut Heap,
    thread: &mut Thread,
    func: RawValue,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    let RawValue::Function(closure) = func else {
        // A non-function callee dispatches its `__call` metamethod, which must
        // itself be a function (`tryfuncTM`, one level); the callee becomes the
        // first argument.
        let handler = tm::get_metamethod(heap, func, MetaEvent::Call)?
            .filter(|h| matches!(h, RawValue::Function(_)))
            .ok_or_else(|| err(call_type_error(func)))?;
        let mut full = Vec::with_capacity(args.len() + 1);
        full.push(func);
        full.extend_from_slice(args);
        return call_value(heap, thread, handler, &full);
    };
    let proto = closure_proto(heap, closure)?;
    // An engine builtin runs synchronously and returns its results directly.
    if let Some(builtin) = heap.proto(proto).and_then(|p| p.native) {
        return builtins::dispatch(
            builtin,
            builtins::BuiltinCallSite::Native,
            func,
            heap,
            thread,
            args,
        );
    }
    // A registered host function runs synchronously here, like a builtin. An
    // async (`Pending`) host call cannot suspend across this native re-entry —
    // the same restriction as yielding across a C-call boundary — so it errors;
    // the async driver only suspends a host call reached directly by `precall`.
    if let Some(host_id) = heap.proto(proto).and_then(|p| p.host) {
        let host = dispatch_host(heap, thread, host_id, args)?;
        return match host {
            DispatchedHostCall::Raw { call, pins } => match call {
                HostCall::Ready(Ok(results)) => {
                    // Native re-entry returns every result to its Rust caller (multret).
                    let materialized = materialize_sync_results(heap, &results, results.len());
                    release_host_pins(heap, pins);
                    materialized
                }
                HostCall::Ready(Err(unwind)) => {
                    let error = host_unwind_to_error(heap, unwind);
                    release_host_pins(heap, pins);
                    Err(error)
                }
                HostCall::Pending(_) => {
                    release_host_pins(heap, pins);
                    Err(err(
                        "attempt to await an async host call across a C-call boundary",
                    ))
                }
            },
            DispatchedHostCall::Scoped(results) => results,
            DispatchedHostCall::AsyncScoped { .. } => Err(err(
                "attempt to await an async host call across a C-call boundary",
            )),
        };
    }
    let (num_params, is_vararg, max_stack) = heap
        .proto(proto)
        .map(|p| {
            (
                u32::from(p.num_params),
                p.is_vararg,
                u32::from(p.max_stack_size).max(1),
            )
        })
        .ok_or_else(|| err("callee has no prototype"))?;

    // Stage func + args just above the active frame's register window.
    let func_reg = thread
        .call_stack
        .iter()
        .rev()
        .find_map(|entry| entry.frame().map(|frame| frame.frame_top))
        .unwrap_or(thread.top);
    let callee_base = func_reg + 1;
    let nargs = u32::try_from(args.len()).unwrap_or(u32::MAX);
    // A variadic callee keeps the arguments past its fixed parameters as metered
    // varargs after its registers are reused.
    let varargs = if is_vararg {
        capture_varargs_from_slice(heap, args, num_params as usize)?
    } else {
        empty_varargs(heap)
    };
    let frame_top = callee_base + max_stack;
    thread
        .stacks
        .ensure(frame_top)
        .map_err(|_| err_register_stack_oom())?;
    thread.stacks.set(func_reg, func);
    for (i, &arg) in args.iter().enumerate() {
        thread
            .stacks
            .set(callee_base + u32::try_from(i).unwrap_or(u32::MAX), arg);
    }
    for i in nargs..num_params {
        thread.stacks.set(callee_base + i, RawValue::Nil);
    }

    let floor = thread.call_stack.len();
    let saved_top = thread.top;
    reserve_call_entries(heap, thread, 1)?;
    thread.top = frame_top;
    thread.push_reserved_call_stack_entry(CallStackEntry::Frame(CallInfo {
        closure,
        proto,
        base: callee_base,
        result_base: func_reg,
        frame_top,
        savedpc: 0,
        nresults: -1,
        varargs,
    }));
    // This nested `dispatch` is a fresh Rust stack frame; bound the re-entry so a
    // chain of function metamethods cannot overflow the host stack.
    thread.native_depth += 1;
    if thread.native_depth > heap.limits().max_native_depth {
        thread.native_depth -= 1;
        return Err(err("stack overflow"));
    }
    // Native re-entry on the same taken-out thread: the caller's unrooted
    // temporaries may be live on the Rust stack, so this cannot root active GC, nor
    // yield the worker.
    let outcome = dispatch(heap, thread, floor, DispatchMode::NativeReentry);
    thread.native_depth -= 1;
    let results = match outcome? {
        Step::Return(results) => results,
        // A metamethod or builtin runs on the Rust stack, which cannot suspend;
        // neither yielding nor awaiting an async host call can cross it.
        Step::Yield(_) => return Err(err("attempt to yield across a C-call boundary")),
        Step::Suspend(_) => {
            return Err(err(
                "attempt to await an async host call across a C-call boundary",
            ));
        }
        Step::SuspendRequire(require) => {
            builtins::release_suspended_require(heap, require);
            return Err(err(builtins::ASYNC_REQUIRE_SYNC_ENTRY_ERROR));
        }
        // Preemption is disabled for this nested dispatch, so it cannot occur.
        Step::Preempt => return Err(err("unexpected preemption in a nested call")),
    };
    thread.top = saved_top;
    Ok(results)
}

/// Calls `func` with `args` in a protected scope, returning its results or the
/// located [`RaisedError`] of an uncaught raise. On an unwind it restores the
/// thread — closing upvalues over the abandoned frames, truncating them, and
/// resetting the stack top — so the caller can keep using the VM. The host entry
/// [`run_function`] and the `pcall` builtin share this; the error is returned
/// un-materialized so the caller can inspect its [`RuntimeErrorKind`] to decide
/// whether to catch it (a fatal cancellation/deadline must propagate).
pub fn protected_call(
    heap: &mut Heap,
    thread: &mut Thread,
    func: RawValue,
    args: &[RawValue],
) -> Result<Vec<RawValue>, RaisedError> {
    protected_call_inner(heap, thread, func, args, None).map_err(|failure| failure.error)
}

/// Calls `func` with `args` in a protected scope and captures a byte-capped
/// traceback before unwinding a catchable failure.
pub fn protected_call_with_traceback(
    heap: &mut Heap,
    thread: &mut Thread,
    func: RawValue,
    args: &[RawValue],
    max_traceback_bytes: usize,
) -> Result<Vec<RawValue>, ProtectedFailure> {
    protected_call_inner(heap, thread, func, args, Some(max_traceback_bytes))
}

fn protected_call_inner(
    heap: &mut Heap,
    thread: &mut Thread,
    func: RawValue,
    args: &[RawValue],
    traceback_limit: Option<usize>,
) -> Result<Vec<RawValue>, ProtectedFailure> {
    let floor = thread.call_stack.len();
    let saved_top = thread.top;
    match call_value(heap, thread, func, args) {
        Ok(results) => Ok(results),
        Err(error) => Err(unwind_protected_failure(
            heap,
            thread,
            floor,
            saved_top,
            error,
            traceback_limit,
        )),
    }
}

/// Calls `func` with `args` from the host as a fresh protected invocation,
/// returning its results or the [`Unwind`] of an uncaught error. Unlike
/// [`call_value`] (the nested metamethod path), this restores the thread's frame
/// depth and stack top on an unwind, so the host can keep using the VM.
///
/// # Errors
/// Returns the [`Unwind`] of an uncaught runtime error.
pub fn run_function(
    heap: &mut Heap,
    thread: &mut Thread,
    func: RawValue,
    args: &[RawValue],
) -> Result<Vec<RawValue>, Unwind> {
    // The host hands `func`/`args` as raw values, so validate each one's heap
    // handle resolves to a live object in this VM before it enters the register
    // stack or is called — a dangling, stale, or cross-VM handle must not be
    // dispatched (the same liveness guard the synchronous host-result boundary
    // applies; this is the `Vm::call_function` entry point the boundary bypasses).
    if let Err(message) = validate_call_inputs(heap, func, args) {
        let error = err(message);
        let kind = error.kind;
        return Err(Unwind {
            error: materialize(heap, error),
            kind,
        });
    }
    match protected_call(heap, thread, func, args) {
        Ok(results) => Ok(results),
        Err(error) => {
            let kind = error.kind;
            Err(Unwind {
                error: materialize(heap, error),
                kind,
            })
        }
    }
}

/// Liveness-checks the raw call target and arguments a host passes to
/// [`run_function`], reusing [`Heap::validate_host_value`]'s per-variant resolve
/// but with call-site wording. Rejects a forged, stale, or cross-VM handle.
pub fn validate_call_inputs(
    heap: &Heap,
    func: RawValue,
    args: &[RawValue],
) -> Result<(), &'static str> {
    heap.validate_host_value(func)
        .map_err(|_| "call target is a forged, stale, or cross-VM heap handle")?;
    for &arg in args {
        heap.validate_host_value(arg)
            .map_err(|_| "call argument is a forged, stale, or cross-VM heap handle")?;
    }
    Ok(())
}

/// How a `CALL` resolved: a frame was pushed or a builtin ran ([`Done`]), a
/// suspendable segment spent its cooperative quantum ([`Preempt`]), the callee
/// was `coroutine.yield`, suspending the thread ([`Yield`]), or the callee was an
/// async host function whose future is pending ([`Suspend`]).
///
/// [`Done`]: PrecallStep::Done
/// [`Preempt`]: PrecallStep::Preempt
/// [`Yield`]: PrecallStep::Yield
/// [`Suspend`]: PrecallStep::Suspend
pub enum PrecallStep {
    /// Continue the dispatch loop (frame pushed, or builtin results placed).
    Done,
    /// A suspendable segment spent its cooperative quantum; dispatch returns
    /// [`Step::Preempt`](crate::state::Step::Preempt) without advancing the CALL pc.
    Preempt,
    /// `coroutine.yield` — `dispatch` returns [`Step::Yield`](crate::state::Step::Yield).
    Yield(Vec<RawValue>),
    /// A coroutine reached a `require` already being loaded by another coroutine.
    /// Dispatch yields no values without advancing the call pc; the next resume
    /// retries the same `require`, hitting the cache or becoming the retrying
    /// leader after the original load finishes.
    WaitForInFlight,
    /// An async host call is pending — `dispatch` returns
    /// [`Step::Suspend`](crate::state::Step::Suspend) and the driver awaits it.
    Suspend(SuspendedCall),
    /// A runtime `require` source operation is pending — `dispatch` returns
    /// [`Step::SuspendRequire`](crate::state::Step::SuspendRequire).
    SuspendRequire(SuspendedRequire),
}

/// Sets up a Lua call frame from a `CALL` at register `base + A`. The argument
/// count comes from `B` (or the live `top` when `B == 0`); the result contract
/// comes from `C`.
pub fn precall(
    heap: &mut Heap,
    thread: &mut Thread,
    base: u32,
    instr: &Instruction,
    preemptible: bool,
) -> Exec<PrecallStep> {
    let func_reg = base + u32::from(instr.a);
    let callee_base = func_reg + 1;
    let mut nargs = if instr.b == 0 {
        thread.top.saturating_sub(callee_base)
    } else {
        u32::from(instr.b) - 1
    };
    // Resolve `__call`: a non-function callee is replaced by its `__call` handler,
    // with the original value inserted as the first argument.
    let callee = resolve_callable(heap, thread, func_reg, &mut nargs)?;

    let proto = closure_proto(heap, callee)?;
    // One proto read classifies the callee. The common case — a plain Lua
    // closure — goes straight to frame setup without touching the builtin or
    // host ladders.
    match heap
        .proto(proto)
        .map_or((None, None), |p| (p.native, p.host))
    {
        (None, None) => {
            push_lua_frame(heap, thread, callee, proto, func_reg, nargs, instr.c)?;
            Ok(PrecallStep::Done)
        }
        (Some(builtin), _) => precall_builtin(
            heap,
            thread,
            base,
            instr,
            preemptible,
            builtin,
            callee,
            nargs,
        ),
        (None, Some(host_id)) => precall_host(heap, thread, host_id, func_reg, nargs, instr.c),
    }
}

/// Dispatches a `CALL` whose callee is an engine builtin. A builtin runs
/// synchronously, with no register frame: its results land at `func_reg` and
/// the `CALL`'s `continue` resumes the caller — except for the special cases
/// with suspension contracts: `coroutine.resume`/`pcall`/`xpcall` push their
/// protected machinery, `coroutine.yield` suspends the thread, `require` may
/// wait for an in-flight load or suspend on its source, and the
/// conformance-harness continuations model upstream's yieldable C closures.
#[allow(clippy::too_many_arguments)]
#[inline]
fn precall_builtin(
    heap: &mut Heap,
    thread: &mut Thread,
    base: u32,
    instr: &Instruction,
    preemptible: bool,
    builtin: builtins::Builtin,
    callee: RawGc<marker::Closure>,
    nargs: u32,
) -> Exec<PrecallStep> {
    let func_reg = base + u32::from(instr.a);
    let callee_base = func_reg + 1;
    if builtin == builtins::Builtin::CoroutineResume {
        let args: Vec<RawValue> = (0..nargs)
            .map(|i| thread.stacks.get(callee_base + i))
            .collect();
        return crate::coroutine::resume_precal(
            heap,
            thread,
            func_reg,
            instr.c,
            &args,
            preemptible,
        );
    }
    if builtin == builtins::Builtin::Pcall {
        return push_pcall_frame(heap, thread, func_reg, callee_base, nargs, instr.c);
    }
    if builtin == builtins::Builtin::Xpcall {
        return push_xpcall_frame(heap, thread, func_reg, callee_base, nargs, instr.c);
    }
    // `coroutine.yield` suspends instead of returning: record where the next
    // resume writes its values (this call's result registers), and report the
    // yielded values up to the coroutine driver.
    if builtin == builtins::Builtin::CoroutineYield {
        thread.resume_slot = Some(ResumeSlot::Direct {
            result_base: func_reg,
            result_count: instr.c,
        });
        let args = collect_stack_results(heap, thread, callee_base, nargs, "coroutine")?;
        return Ok(PrecallStep::Yield(args));
    }
    let args: Vec<RawValue> = (0..nargs)
        .map(|i| thread.stacks.get(callee_base + i))
        .collect();
    if builtin == builtins::Builtin::Require {
        return match builtins::start_require(
            heap,
            thread,
            &args,
            &builtins::RequireCallSite {
                result_reg: func_reg,
                result_count: instr.c,
                cleanup_end: callee_base.saturating_add(nargs),
            },
        )? {
            builtins::RequireCallStep::Ready(results) => {
                place_results(heap, thread, func_reg, instr.c, &results)?;
                Ok(PrecallStep::Done)
            }
            builtins::RequireCallStep::WaitForInFlight => Ok(PrecallStep::WaitForInFlight),
            builtins::RequireCallStep::Suspend(require) => Ok(PrecallStep::SuspendRequire(require)),
            builtins::RequireCallStep::BodyStarted => Ok(PrecallStep::Done),
        };
    }
    if let Some(step) = start_conformance_native_continuation(
        heap, thread, base, builtin, func_reg, instr.c, &args,
    )? {
        return Ok(step);
    }
    let results = builtins::dispatch(
        builtin,
        builtins::BuiltinCallSite::Bytecode,
        RawValue::Function(callee),
        heap,
        thread,
        &args,
    )?;
    place_results(heap, thread, func_reg, instr.c, &results)?;
    Ok(PrecallStep::Done)
}

/// Dispatches a `CALL` whose callee is a registered host function, through the
/// host-call ABI. Like a builtin it has no register frame; its synchronous
/// (`Ready`) results land at `func_reg`, and a `Pending` (await) result
/// suspends to the async driver. Registry pins taken during the call are
/// released only after the result — or error — value has been materialized.
#[inline]
fn precall_host(
    heap: &mut Heap,
    thread: &mut Thread,
    host_id: HostId,
    func_reg: u32,
    nargs: u32,
    result_count: u8,
) -> Exec<PrecallStep> {
    let callee_base = func_reg + 1;
    let args: Vec<RawValue> = (0..nargs)
        .map(|i| thread.stacks.get(callee_base + i))
        .collect();
    let host = dispatch_host(heap, thread, host_id, &args)?;
    match host {
        DispatchedHostCall::Raw { call, pins } => match call {
            HostCall::Ready(Ok(results)) => {
                // A fixed-arity `CALL` observes only `C - 1` results; materialize just
                // those so an ignored tail is never interned or rejected (parity with
                // the async `materialize_return`).
                let want = observed_results(result_count, results.len());
                let placed = materialize_sync_results(heap, &results, want).and_then(|values| {
                    place_results(heap, thread, func_reg, result_count, &values)
                });
                release_host_pins(heap, pins);
                placed?;
                Ok(PrecallStep::Done)
            }
            HostCall::Ready(Err(unwind)) => {
                let error = host_unwind_to_error(heap, unwind);
                release_host_pins(heap, pins);
                Err(error)
            }
            // The driver awaits the future off the VM borrow and resumes the
            // suspended `CALL` by placing the materialized result at `func_reg`.
            // The `CALL` arm records the resume `savedpc` and the call-site `pc`.
            HostCall::Pending(future) => Ok(PrecallStep::Suspend(SuspendedCall {
                future,
                host_requests: None,
                pins,
                result_reg: func_reg,
                result_count,
                call_pc: 0,
                cleanup_end: callee_base.saturating_add(nargs),
                target: SuspendedTarget::Active,
            })),
        },
        DispatchedHostCall::Scoped(results) => {
            // `place_results` honors the `CALL` `C` operand itself: it copies at
            // most `C - 1` values and pads a shortfall with nil, so the full
            // result list is passed through unsliced.
            let results = results?;
            place_results(heap, thread, func_reg, result_count, &results)?;
            Ok(PrecallStep::Done)
        }
        DispatchedHostCall::AsyncScoped {
            future,
            host_requests,
        } => Ok(PrecallStep::Suspend(SuspendedCall {
            future,
            host_requests: Some(host_requests),
            pins: Vec::new(),
            result_reg: func_reg,
            result_count,
            call_pc: 0,
            cleanup_end: callee_base.saturating_add(nargs),
            target: SuspendedTarget::Active,
        })),
    }
}

/// Pushes the [`CallInfo`] frame for a `CALL` of a plain Lua closure: captures
/// the variadic tail through metered side storage, nil-fills missing fixed
/// parameters, reserves the register and call stacks, and pushes the frame the
/// dispatch loop continues in.
#[inline]
fn push_lua_frame(
    heap: &Heap,
    thread: &mut Thread,
    callee: RawGc<marker::Closure>,
    proto: RawGc<Proto>,
    func_reg: u32,
    nargs: u32,
    result_count: u8,
) -> Exec<()> {
    let callee_base = func_reg + 1;
    let (num_params, is_vararg, max_stack) = heap
        .proto(proto)
        .map(|p| {
            (
                u32::from(p.num_params),
                p.is_vararg,
                u32::from(p.max_stack_size).max(1),
            )
        })
        .ok_or_else(|| err("callee has no prototype"))?;

    // A variadic callee keeps the arguments beyond its fixed parameters as the
    // frame's varargs (what `...` reads); they sit in registers the body will
    // reuse, so capture them through metered side storage before that happens.
    let varargs = if is_vararg && nargs > num_params {
        capture_varargs_from_stack(heap, thread, callee_base + num_params, callee_base + nargs)?
    } else {
        empty_varargs(heap)
    };
    for i in nargs..num_params {
        thread.stacks.set(callee_base + i, RawValue::Nil);
    }
    let frame_top = callee_base + max_stack;
    thread
        .stacks
        .ensure(frame_top)
        .map_err(|_| err_register_stack_oom())?;
    reserve_call_entries(heap, thread, 1)?;
    thread.top = frame_top;

    thread.push_reserved_call_stack_entry(CallStackEntry::Frame(CallInfo {
        closure: callee,
        proto,
        base: callee_base,
        result_base: func_reg,
        frame_top,
        savedpc: 0,
        nresults: if result_count == 0 {
            -1
        } else {
            i32::from(result_count) - 1
        },
        varargs,
    }));
    Ok(())
}

/// Result of resuming a harness-only native continuation.
pub enum ConformanceNativeStep {
    /// The continuation yielded again.
    Yield(Vec<RawValue>),
    /// The continuation completed with return values.
    Return(Vec<RawValue>),
}

fn start_conformance_native_continuation(
    heap: &mut Heap,
    thread: &mut Thread,
    base: u32,
    builtin: builtins::Builtin,
    result_base: u32,
    result_count: u8,
    args: &[RawValue],
) -> Exec<Option<PrecallStep>> {
    match builtin {
        builtins::Builtin::ConformanceSingleYield => {
            thread.resume_slot = Some(ResumeSlot::ConformanceNative {
                result_base,
                result_count,
                continuation: ConformanceNativeContinuation::SingleYield,
            });
            Ok(Some(PrecallStep::Yield(vec![RawValue::Number(2.0)])))
        }
        builtins::Builtin::ConformanceMultipleYields => {
            let base = number_arg(
                args.first().copied().unwrap_or(RawValue::Nil),
                "multipleYields",
            )?;
            thread.resume_slot = Some(ResumeSlot::ConformanceNative {
                result_base,
                result_count,
                continuation: ConformanceNativeContinuation::MultipleYields { base, pos: 1 },
            });
            Ok(Some(PrecallStep::Yield(vec![RawValue::Number(base + 1.0)])))
        }
        builtins::Builtin::ConformanceMultipleYieldsWithNestedCall => {
            let base = number_arg(
                args.first().copied().unwrap_or(RawValue::Nil),
                "multipleYieldsWithNestedCall",
            )?;
            let nested_should_yield = matches!(args.get(1), Some(RawValue::Boolean(true)));
            let (state, first_yield) = if nested_should_yield {
                (0, 105.0)
            } else {
                (1, 110.0)
            };
            thread.resume_slot = Some(ResumeSlot::ConformanceNative {
                result_base,
                result_count,
                continuation: ConformanceNativeContinuation::MultipleYieldsWithNestedCall {
                    base,
                    state,
                },
            });
            Ok(Some(PrecallStep::Yield(vec![RawValue::Number(
                first_yield,
            )])))
        }
        builtins::Builtin::ConformancePassthroughCall
        | builtins::Builtin::ConformancePassthroughCallMoreResults
        | builtins::Builtin::ConformancePassthroughCallArgReuse
        | builtins::Builtin::ConformancePassthroughCallVaradic
        | builtins::Builtin::ConformancePassthroughCallWithState => {
            rewrite_passthrough_call(heap, thread, base, builtin, result_base, result_count, args)
                .map(Some)
        }
        _ => Ok(None),
    }
}

fn rewrite_passthrough_call(
    heap: &mut Heap,
    thread: &mut Thread,
    base: u32,
    builtin: builtins::Builtin,
    func_reg: u32,
    result_count: u8,
    args: &[RawValue],
) -> Exec<PrecallStep> {
    let target = args
        .first()
        .copied()
        .ok_or_else(|| err("bad argument #1 to passthrough helper (function expected)"))?;
    let target_args = match builtin {
        builtins::Builtin::ConformancePassthroughCall
        | builtins::Builtin::ConformancePassthroughCallMoreResults
        | builtins::Builtin::ConformancePassthroughCallArgReuse => {
            args.iter().skip(1).take(2).copied().collect::<Vec<_>>()
        }
        builtins::Builtin::ConformancePassthroughCallVaradic
        | builtins::Builtin::ConformancePassthroughCallWithState => {
            args.iter().skip(1).copied().collect::<Vec<_>>()
        }
        _ => unreachable!("only passthrough helpers reach this function"),
    };
    let target_result_count = match builtin {
        // The C helpers request one/ten internal results and then their
        // continuation returns the first value. The observable result shape is a
        // single value.
        builtins::Builtin::ConformancePassthroughCall
        | builtins::Builtin::ConformancePassthroughCallMoreResults
        | builtins::Builtin::ConformancePassthroughCallArgReuse => 2,
        // The variadic helpers return every target result their caller observes.
        builtins::Builtin::ConformancePassthroughCallVaradic
        | builtins::Builtin::ConformancePassthroughCallWithState => result_count,
        _ => unreachable!("only passthrough helpers reach this function"),
    };
    let target_nargs = target_args
        .len()
        .checked_add(1)
        .and_then(|count| u8::try_from(count).ok())
        .ok_or_else(|| err("too many arguments to passthrough helper"))?;
    thread
        .stacks
        .ensure(func_reg + u32::from(target_nargs))
        .map_err(|_| err_memory("not enough memory for passthrough call"))?;
    thread.stacks.set(func_reg, target);
    for (index, value) in target_args.into_iter().enumerate() {
        thread.stacks.set(
            func_reg + 1 + u32::try_from(index).unwrap_or(u32::MAX),
            value,
        );
    }
    thread.top = func_reg + u32::from(target_nargs);
    let nested = Instruction::abc(
        Opcode::Call,
        u8::try_from(func_reg.saturating_sub(base))
            .map_err(|_| err("passthrough call register out of range"))?,
        target_nargs,
        target_result_count,
    );
    precall(heap, thread, base, &nested, false)
}

/// Resumes a harness-only native continuation. This models upstream's
/// `lua_pushcclosurek` helpers used by `cyield.luau`; production host calls still
/// use the `HostFunction` ABI.
pub fn resume_conformance_native_continuation(
    thread: &mut Thread,
    result_base: u32,
    result_count: u8,
    continuation: &ConformanceNativeContinuation,
) -> Exec<ConformanceNativeStep> {
    match continuation {
        ConformanceNativeContinuation::SingleYield => {
            Ok(ConformanceNativeStep::Return(vec![RawValue::Number(4.0)]))
        }
        ConformanceNativeContinuation::MultipleYields { base, pos } => {
            let base = *base;
            let next = pos + 1;
            if next < 4 {
                thread.resume_slot = Some(ResumeSlot::ConformanceNative {
                    result_base,
                    result_count,
                    continuation: ConformanceNativeContinuation::MultipleYields { base, pos: next },
                });
                Ok(ConformanceNativeStep::Yield(vec![RawValue::Number(
                    base + next as f64,
                )]))
            } else {
                Ok(ConformanceNativeStep::Return(vec![RawValue::Number(
                    base + next as f64,
                )]))
            }
        }
        ConformanceNativeContinuation::MultipleYieldsWithNestedCall { base, state } => {
            let base = *base;
            match *state {
                0 => {
                    thread.resume_slot = Some(ResumeSlot::ConformanceNative {
                        result_base,
                        result_count,
                        continuation: ConformanceNativeContinuation::MultipleYieldsWithNestedCall {
                            base,
                            state: 1,
                        },
                    });
                    Ok(ConformanceNativeStep::Yield(vec![RawValue::Number(115.0)]))
                }
                1 => {
                    thread.resume_slot = Some(ResumeSlot::ConformanceNative {
                        result_base,
                        result_count,
                        continuation: ConformanceNativeContinuation::MultipleYieldsWithNestedCall {
                            base,
                            state: 2,
                        },
                    });
                    Ok(ConformanceNativeStep::Yield(vec![RawValue::Number(
                        base + 200.0,
                    )]))
                }
                _ => Ok(ConformanceNativeStep::Return(vec![RawValue::Number(
                    base + 210.0,
                )])),
            }
        }
    }
}

fn number_arg(value: RawValue, name: &str) -> Exec<f64> {
    match value {
        RawValue::Integer(i) => Ok(i as f64),
        RawValue::Number(n) => Ok(n),
        _ => Err(err(format!(
            "bad argument #1 to '{name}' (number expected)"
        ))),
    }
}

fn push_pcall_frame(
    heap: &mut Heap,
    thread: &mut Thread,
    result_base: u32,
    target_reg: u32,
    nargs: u32,
    result_count: u8,
) -> Exec<PrecallStep> {
    if nargs == 0 {
        return Err(err("missing value to 'pcall'"));
    }
    if let Err(error) = reserve_call_entries(heap, thread, 3) {
        let error = materialize(heap, error);
        place_protected_results(heap, thread, result_base, result_count, false, &[error])?;
        return Ok(PrecallStep::Done);
    }

    let target_nargs = nargs - 1;
    match prepare_protected_target(heap, thread, target_reg, target_nargs) {
        Ok(ProtectedTarget::Frame(target_frame)) => {
            let saved_top = thread.top;
            thread.push_reserved_call_stack_entry(CallStackEntry::Protected(ProtectedInfo {
                result_base,
                result_count,
                saved_top,
                close_base: target_frame.base,
                handler: None,
            }));
            thread.top = target_frame.frame_top;
            thread.push_reserved_call_stack_entry(CallStackEntry::Frame(target_frame));
        }
        Ok(ProtectedTarget::Pcall {
            func_reg,
            nargs: pcall_nargs,
        }) => {
            thread.push_reserved_call_stack_entry(CallStackEntry::Protected(ProtectedInfo {
                result_base,
                result_count,
                saved_top: thread.top,
                close_base: func_reg + 1,
                handler: None,
            }));
            match push_pcall_frame(heap, thread, func_reg, func_reg + 1, pcall_nargs, 0) {
                Ok(step) => return Ok(step),
                Err(error) => {
                    catch_protected_error(
                        heap,
                        thread,
                        thread.call_stack.len().saturating_sub(1),
                        error,
                    )?;
                }
            }
        }
        Ok(ProtectedTarget::Results(results)) => {
            complete_protected_results(heap, thread, result_base, result_count, true, &results)?;
        }
        Ok(ProtectedTarget::Yield(values)) => {
            if thread.entry.is_none() {
                let error = materialize(
                    heap,
                    err("attempt to yield across a protected-call boundary"),
                );
                complete_protected_results(
                    heap,
                    thread,
                    result_base,
                    result_count,
                    false,
                    &[error],
                )?;
                return Ok(PrecallStep::Done);
            }
            thread.resume_slot = Some(ResumeSlot::Protected {
                result_base,
                result_count,
            });
            return Ok(PrecallStep::Yield(values));
        }
        // A fatal error from resolving/running the immediate target (e.g.
        // `pcall(some_builtin)` whose target trips cancellation) propagates past the
        // `pcall` rather than being caught into `false, <error>`.
        Err(error) if !error.is_catchable() => return Err(error),
        Err(error) => {
            let error = crate::debug::locate(heap, thread, error);
            let error = materialize(heap, error);
            complete_protected_results(heap, thread, result_base, result_count, false, &[error])?;
        }
    }
    Ok(PrecallStep::Done)
}

fn push_xpcall_frame(
    heap: &mut Heap,
    thread: &mut Thread,
    result_base: u32,
    target_reg: u32,
    nargs: u32,
    result_count: u8,
) -> Exec<PrecallStep> {
    if nargs < 2 {
        return Err(err_no_location(
            "missing argument #2 to 'xpcall' (function expected)",
        ));
    }
    let handler = match thread.stacks.get(target_reg + 1) {
        RawValue::Function(handler) => RawValue::Function(handler),
        other => {
            return Err(err_no_location(format!(
                "invalid argument #2 to 'xpcall' (function expected, got {})",
                String::from_utf8_lossy(builtins::type_name(other))
            )));
        }
    };
    if let Err(error) = reserve_call_entries(heap, thread, 3) {
        let error_kind = error.kind;
        let error = materialize(heap, error);
        let handler_result = run_xpcall_handler(heap, thread, handler, error, error_kind)?;
        place_protected_results(
            heap,
            thread,
            result_base,
            result_count,
            false,
            &[handler_result],
        )?;
        return Ok(PrecallStep::Done);
    }

    let target_nargs = nargs - 2;
    for i in 0..target_nargs {
        let value = thread.stacks.get(target_reg + 2 + i);
        thread.stacks.set(target_reg + 1 + i, value);
    }
    let boundary_floor = thread.call_stack.len();
    thread.push_reserved_call_stack_entry(CallStackEntry::Protected(ProtectedInfo {
        result_base,
        result_count,
        saved_top: thread.top,
        close_base: target_reg + 1,
        handler: Some(handler),
    }));

    match prepare_protected_target(heap, thread, target_reg, target_nargs) {
        Ok(ProtectedTarget::Frame(target_frame)) => {
            thread.top = target_frame.frame_top;
            thread.push_reserved_call_stack_entry(CallStackEntry::Frame(target_frame));
        }
        Ok(ProtectedTarget::Pcall {
            func_reg,
            nargs: pcall_nargs,
        }) => match push_pcall_frame(heap, thread, func_reg, func_reg + 1, pcall_nargs, 0) {
            Ok(step) => return Ok(step),
            Err(error) => {
                catch_protected_error(heap, thread, boundary_floor, error)?;
            }
        },
        Ok(ProtectedTarget::Results(results)) => {
            let protected = match thread.call_stack.pop() {
                Some(CallStackEntry::Protected(protected)) => protected,
                _ => unreachable!("xpcall boundary is on top before immediate completion"),
            };
            thread.top = protected.saved_top;
            complete_protected_results(
                heap,
                thread,
                protected.result_base,
                protected.result_count,
                true,
                &results,
            )?;
        }
        Ok(ProtectedTarget::Yield(values)) => {
            if thread.entry.is_none() {
                catch_protected_error(
                    heap,
                    thread,
                    boundary_floor,
                    err("attempt to yield across a protected-call boundary"),
                )?;
                return Ok(PrecallStep::Done);
            }
            let protected = match thread.call_stack.pop() {
                Some(CallStackEntry::Protected(protected)) => protected,
                _ => unreachable!("xpcall boundary is on top before immediate yield"),
            };
            thread.resume_slot = Some(ResumeSlot::Protected {
                result_base: protected.result_base,
                result_count: protected.result_count,
            });
            return Ok(PrecallStep::Yield(values));
        }
        Err(error) => {
            catch_protected_error(heap, thread, boundary_floor, error)?;
        }
    }
    Ok(PrecallStep::Done)
}

enum ProtectedTarget {
    Frame(CallInfo),
    Pcall { func_reg: u32, nargs: u32 },
    Results(Vec<RawValue>),
    Yield(Vec<RawValue>),
}

fn prepare_protected_target(
    heap: &mut Heap,
    thread: &mut Thread,
    func_reg: u32,
    nargs: u32,
) -> Exec<ProtectedTarget> {
    let callee_base = func_reg + 1;
    let mut target_nargs = nargs;
    let callee = resolve_callable(heap, thread, func_reg, &mut target_nargs)?;
    let proto = closure_proto(heap, callee)?;
    if let Some(builtin) = heap.proto(proto).and_then(|p| p.native) {
        if builtin == builtins::Builtin::CoroutineYield {
            let args = collect_stack_results(heap, thread, callee_base, target_nargs, "coroutine")?;
            return Ok(ProtectedTarget::Yield(args));
        }
        if builtin == builtins::Builtin::Pcall {
            return Ok(ProtectedTarget::Pcall {
                func_reg,
                nargs: target_nargs,
            });
        }
        let args: Vec<RawValue> = (0..target_nargs)
            .map(|i| thread.stacks.get(callee_base + i))
            .collect();
        return run_immediate_protected_target(heap, thread, |heap, thread| {
            builtins::dispatch(
                builtin,
                builtins::BuiltinCallSite::Native,
                RawValue::Function(callee),
                heap,
                thread,
                &args,
            )
        })
        .map(ProtectedTarget::Results);
    }
    let args: Vec<RawValue> = (0..target_nargs)
        .map(|i| thread.stacks.get(callee_base + i))
        .collect();
    if let Some(host_id) = heap.proto(proto).and_then(|p| p.host) {
        let host = dispatch_host(heap, thread, host_id, &args)?;
        return match host {
            DispatchedHostCall::Raw { call, pins } => match call {
                HostCall::Ready(Ok(results)) => {
                    // The protected target observes every result; the caller applies arity.
                    let materialized = materialize_sync_results(heap, &results, results.len());
                    release_host_pins(heap, pins);
                    Ok(ProtectedTarget::Results(materialized?))
                }
                HostCall::Ready(Err(unwind)) => {
                    let error = host_unwind_to_error(heap, unwind);
                    release_host_pins(heap, pins);
                    Err(error)
                }
                HostCall::Pending(_) => {
                    release_host_pins(heap, pins);
                    Err(err(
                        "attempt to await an async host call across a C-call boundary",
                    ))
                }
            },
            DispatchedHostCall::Scoped(results) => results.map(ProtectedTarget::Results),
            DispatchedHostCall::AsyncScoped { .. } => Err(err(
                "attempt to await an async host call across a C-call boundary",
            )),
        };
    }
    let (num_params, is_vararg, max_stack) = heap
        .proto(proto)
        .map(|p| {
            (
                u32::from(p.num_params),
                p.is_vararg,
                u32::from(p.max_stack_size).max(1),
            )
        })
        .ok_or_else(|| err("callee has no prototype"))?;
    let varargs = if is_vararg && target_nargs > num_params {
        capture_varargs_from_stack(
            heap,
            thread,
            callee_base + num_params,
            callee_base + target_nargs,
        )?
    } else {
        empty_varargs(heap)
    };
    for i in target_nargs..num_params {
        thread.stacks.set(callee_base + i, RawValue::Nil);
    }
    let frame_top = callee_base + max_stack;
    thread
        .stacks
        .ensure(frame_top)
        .map_err(|_| err_register_stack_oom())?;
    Ok(ProtectedTarget::Frame(CallInfo {
        closure: callee,
        proto,
        base: callee_base,
        result_base: func_reg,
        frame_top,
        savedpc: 0,
        nresults: -1,
        varargs,
    }))
}

fn run_immediate_protected_target(
    heap: &mut Heap,
    thread: &mut Thread,
    run: impl FnOnce(&mut Heap, &mut Thread) -> Exec<Vec<RawValue>>,
) -> Exec<Vec<RawValue>> {
    let floor = thread.call_stack.len();
    let saved_top = thread.top;
    match run(heap, thread) {
        Ok(results) => Ok(results),
        Err(error) => {
            close_upvalues(heap, thread, floor);
            truncate_call_stack(heap, thread, floor);
            thread.top = saved_top;
            Err(error)
        }
    }
}

/// Writes a synchronous call's results into the caller's window at `result_base`,
/// honoring the `CALL` `C` operand (a fixed count `c - 1`, or every result for
/// multret `c == 0`), then resets the live top.
pub fn place_results(
    heap: &mut Heap,
    thread: &mut Thread,
    result_base: u32,
    c: u8,
    results: &[RawValue],
) -> Exec<()> {
    let produced = u32::try_from(results.len()).unwrap_or(u32::MAX);
    let want = if c == 0 { produced } else { u32::from(c) - 1 };
    prepare_result_copy(heap, want as usize, "call")?;
    // Grow the register file through the accounted `ensure` before writing, so a
    // large multret result cannot resize the stack on the unmetered `set` path —
    // a host returning an enormous result list raises here instead of OOM-panicking.
    thread
        .stacks
        .ensure(result_base.saturating_add(want))
        .map_err(|_| err_register_stack_oom())?;
    for i in 0..want {
        let value = results.get(i as usize).copied().unwrap_or(RawValue::Nil);
        thread.stacks.set(result_base + i, value);
    }
    thread.top = if c == 0 {
        result_base + produced
    } else {
        thread
            .call_stack
            .iter()
            .rev()
            .find_map(|entry| entry.frame().map(|frame| frame.frame_top))
            .unwrap_or(result_base + want)
    };
    Ok(())
}

pub fn clear_call_temps(thread: &mut Thread, result_base: u32, observed: usize, end: u32) {
    let start = result_base.saturating_add(u32::try_from(observed).unwrap_or(u32::MAX));
    for slot in start..end {
        thread.stacks.set(slot, RawValue::Nil);
    }
}

pub fn place_protected_results(
    heap: &mut Heap,
    thread: &mut Thread,
    result_base: u32,
    c: u8,
    ok: bool,
    results: &[RawValue],
) -> Exec<u32> {
    let produced = u32::try_from(results.len()).unwrap_or(u32::MAX);
    let total = produced.saturating_add(1);
    let want = if c == 0 { total } else { u32::from(c) - 1 };
    prepare_result_copy(heap, want as usize, "protected-call")?;
    thread
        .stacks
        .ensure(result_base.saturating_add(want))
        .map_err(|_| err_register_stack_oom())?;
    if want > 0 {
        thread.stacks.set(result_base, RawValue::Boolean(ok));
    }
    for i in 1..want {
        let value = results
            .get((i - 1) as usize)
            .copied()
            .unwrap_or(RawValue::Nil);
        thread.stacks.set(result_base + i, value);
    }
    thread.top = if c == 0 {
        result_base + total
    } else {
        thread
            .call_stack
            .iter()
            .rev()
            .find_map(|entry| entry.frame().map(|frame| frame.frame_top))
            .unwrap_or(result_base + want)
    };
    Ok(want)
}

pub fn complete_protected_results(
    heap: &mut Heap,
    thread: &mut Thread,
    mut result_base: u32,
    mut result_count: u8,
    ok: bool,
    results: &[RawValue],
) -> Exec<()> {
    let mut placed = place_protected_results(heap, thread, result_base, result_count, ok, results)?;
    while thread
        .call_stack
        .last()
        .is_some_and(|entry| entry.protected().is_some())
    {
        let protected = match thread.call_stack.pop() {
            Some(CallStackEntry::Protected(protected)) => protected,
            _ => unreachable!("checked protected boundary"),
        };
        let child_results =
            collect_stack_results(heap, thread, result_base, placed, "protected-call")?;
        result_base = protected.result_base;
        result_count = protected.result_count;
        thread.top = protected.saved_top;
        placed = place_protected_results(
            heap,
            thread,
            result_base,
            result_count,
            true,
            &child_results,
        )?;
    }
    Ok(())
}

pub fn has_protected_boundary(thread: &Thread, floor: usize) -> bool {
    thread
        .call_stack
        .iter()
        .enumerate()
        .rev()
        .any(|(index, entry)| index >= floor && entry.protected().is_some())
}

fn run_xpcall_handler(
    heap: &mut Heap,
    thread: &mut Thread,
    handler: RawValue,
    error: RawValue,
    error_kind: RuntimeErrorKind,
) -> Exec<RawValue> {
    match call_value(heap, thread, handler, &[error]) {
        Ok(results) => Ok(results.into_iter().next().unwrap_or(RawValue::Nil)),
        // A handler that raises an ordinary error yields a fixed string. If both
        // the protected function and handler hit memory failure, preserve the
        // original memory error so a secondary allocation failure cannot mask it.
        Err(handler_error) if handler_error.is_catchable() => {
            match (error_kind, handler_error.kind) {
                (RuntimeErrorKind::Memory, RuntimeErrorKind::Memory) => Ok(error),
                _ => Ok(materialize(heap, err_handler_failure())),
            }
        }
        Err(handler_error) => Err(handler_error),
    }
}

pub fn catch_protected_error(
    heap: &mut Heap,
    thread: &mut Thread,
    floor: usize,
    error: RaisedError,
) -> Exec<()> {
    // A fatal error (cancellation, deadline) is uncatchable: propagate it past
    // every protected boundary so a tenant cannot swallow a termination signal.
    if !error.is_catchable() {
        return Err(error);
    }
    let Some(boundary) = thread
        .call_stack
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| {
            (index >= floor)
                .then(|| entry.protected().map(|protected| (index, protected)))
                .flatten()
        })
    else {
        return Err(error);
    };
    let (boundary_index, protected) = boundary;
    let result_base = protected.result_base;
    let result_count = protected.result_count;
    let saved_top = protected.saved_top;
    let close_base = protected.close_base;
    let handler = protected.handler;

    let error_kind = error.kind;
    let error = crate::debug::locate(heap, thread, error);
    let error = materialize(heap, error);
    let handler_outcome =
        handler.map(|handler| run_xpcall_handler(heap, thread, handler, error, error_kind));
    close_upvals_from(heap, thread, close_base);
    truncate_call_stack(heap, thread, boundary_index);
    thread.top = saved_top;
    let replaced = match handler_outcome {
        // The handler ran and produced a replacement error value.
        Some(Ok(value)) => value,
        // The handler itself raised a fatal error: propagate it past the (now
        // unwound) boundary rather than masking it as "error in error handling".
        Some(Err(fatal)) => return Err(fatal),
        // No handler (plain `pcall`): surface the original materialized error.
        None => error,
    };
    complete_protected_results(heap, thread, result_base, result_count, false, &[replaced])?;
    Ok(())
}

/// Runs a registered host function's synchronous part.
///
/// The registry slot is shared (`Arc`), not taken out, so the call can hold
/// `&mut Heap` through [`EngineContext`] without aliasing the registry — and a
/// host function that re-enters the VM and recursively dispatches *itself* (a
/// bound script triggering the host call that ran it) resolves the same slot
/// again instead of finding it empty. A panicking host function unwinds
/// straight to the host-call boundary's poison guard.
fn dispatch_host(
    heap: &mut Heap,
    thread: &mut Thread,
    host_id: HostId,
    args: &[RawValue],
) -> Exec<DispatchedHostCall> {
    let function = heap
        .host(host_id)
        .ok_or_else(|| err("host function is not registered"))?;
    let dispatched = match &*function {
        HostCallable::Raw(function) => {
            let mut ctx = EngineContext::new(heap, args);
            let call = function.call(&mut ctx);
            DispatchedHostCall::Raw {
                call,
                pins: ctx.into_pins(),
            }
        }
        HostCallable::Scoped(function) => {
            let Some(scope) = scope::Scope::with_active_host_app_data_guard(heap, thread) else {
                return Ok(DispatchedHostCall::Scoped(Err(err(
                    "scoped host function called without an active VM context",
                ))));
            };
            let args = scope::MultiValue::from_raw_values(args.to_vec());
            let result = function
                .call(&scope, args)
                .map(scope::MultiValue::into_raw_vec)
                .map_err(scoped_host_error_to_runtime);
            DispatchedHostCall::Scoped(result)
        }
        HostCallable::Async(function) => {
            let Some(scope) = scope::Scope::with_active_host_app_data_guard(heap, thread) else {
                return Ok(DispatchedHostCall::Scoped(Err(err(
                    "async host function called without an active VM context",
                ))));
            };
            let args = scope::MultiValue::from_raw_values(args.to_vec());
            let (ctx, host_requests) = crate::host::AsyncHostContext::channel();
            match function
                .call(ctx, &scope, args)
                .map_err(scoped_host_error_to_runtime)
            {
                Ok(future) => DispatchedHostCall::AsyncScoped {
                    future,
                    host_requests,
                },
                Err(error) => DispatchedHostCall::Scoped(Err(error)),
            }
        }
    };
    // No handle validation is needed at the boundary: a synchronous result — both the
    // `Ok` values and the `Err` (`HostUnwind`) error — carries only `OwnedValue`, so a
    // raw forged handle is unrepresentable, and the one heap reference it can name
    // (`OwnedValue::Pinned`) is validated by the token-checked registry when the result
    // is materialized (`materialize_sync_results` / `host_unwind_to_error`).
    Ok(dispatched)
}

/// Resolves the value at `func_reg` to a closure to call. A function is returned
/// directly; any other value dispatches its `__call` handler, which must itself
/// be a function (`tryfuncTM`, one level — `__call` is not chained). The original
/// value is shifted in as the call's first argument, so `nargs` grows by one.
fn resolve_callable(
    heap: &Heap,
    thread: &mut Thread,
    func_reg: u32,
    nargs: &mut u32,
) -> Exec<RawGc<marker::Closure>> {
    let callee = thread.stacks.get(func_reg);
    if let RawValue::Function(closure) = callee {
        return Ok(closure);
    }
    let handler = tm::get_metamethod(heap, callee, MetaEvent::Call)?;
    let Some(RawValue::Function(closure)) = handler else {
        return Err(err(call_type_error(callee)));
    };
    thread
        .stacks
        .ensure(func_reg + 2 + *nargs)
        .map_err(|_| err_register_stack_oom())?;
    // Shift the existing args up one slot (right to left), then place the original
    // value as the first argument and the handler as the function.
    for i in (0..*nargs).rev() {
        let arg = thread.stacks.get(func_reg + 1 + i);
        thread.stacks.set(func_reg + 2 + i, arg);
    }
    thread.stacks.set(func_reg + 1, callee);
    thread.stacks.set(func_reg, RawValue::Function(closure));
    *nargs += 1;
    Ok(closure)
}

/// Handles `RETURN`. Returns `Some((result_base, count))` when the call stack
/// has unwound back to `floor` — the protected region's root returned and its
/// results sit at `result_base`. Otherwise resumes the caller, restoring its
/// `top` (multret: just past the copied results; fixed: the caller's frame top).
pub fn return_op(
    heap: &mut Heap,
    thread: &mut Thread,
    floor: usize,
    base: u32,
    instr: &Instruction,
) -> Exec<Option<(u32, u32)>> {
    // Close any upvalues this frame's locals still hold open before its registers
    // are reused (`luaF_close` in poscall), even when the compiler omitted an
    // explicit `CLOSEUPVALS`.
    close_upvals_from(heap, thread, base);
    let frame = thread
        .call_stack
        .pop()
        .and_then(|entry| match entry {
            CallStackEntry::Frame(frame) => Some(frame),
            CallStackEntry::Protected(_) | CallStackEntry::Require(_) => None,
        })
        .ok_or_else(|| err("return with no active frame"))?;
    let first = base + u32::from(instr.a);
    let count = if instr.b == 0 {
        thread.top.saturating_sub(first)
    } else {
        u32::from(instr.b) - 1
    };

    if thread
        .call_stack
        .last()
        .is_some_and(|entry| entry.require().is_some())
    {
        let require = match thread.call_stack.pop() {
            Some(CallStackEntry::Require(require)) => require,
            _ => unreachable!("checked require continuation"),
        };
        let results = collect_stack_results(heap, thread, first, count, "require")?;
        complete_require_results(heap, thread, &require, &results)?;
        return Ok(None);
    }

    if thread
        .call_stack
        .last()
        .is_some_and(|entry| entry.protected().is_some())
    {
        let protected = match thread.call_stack.pop() {
            Some(CallStackEntry::Protected(protected)) => protected,
            _ => unreachable!("checked protected boundary"),
        };
        let results = collect_stack_results(heap, thread, first, count, "protected-call")?;
        thread.top = protected.saved_top;
        complete_protected_results(
            heap,
            thread,
            protected.result_base,
            protected.result_count,
            true,
            &results,
        )?;
        return Ok(None);
    }

    if thread.call_stack.len() == floor {
        prepare_result_copy(heap, count as usize, "return")?;
        for i in 0..count {
            let v = thread.stacks.get(first + i);
            thread.stacks.set(frame.result_base + i, v);
        }
        thread.top = frame.result_base + count;
        return Ok(Some((frame.result_base, count)));
    }

    let want = if frame.nresults < 0 {
        count
    } else {
        u32::try_from(frame.nresults).unwrap_or(0)
    };
    prepare_result_copy(heap, want as usize, "call")?;
    for i in 0..want {
        let v = if i < count {
            thread.stacks.get(first + i)
        } else {
            RawValue::Nil
        };
        thread.stacks.set(frame.result_base + i, v);
    }
    thread.top = if frame.nresults < 0 {
        frame.result_base + count
    } else {
        thread
            .call_stack
            .iter()
            .rev()
            .find_map(|entry| entry.frame().map(|caller| caller.frame_top))
            .unwrap_or(frame.result_base + want)
    };
    Ok(None)
}

fn complete_require_results(
    heap: &mut Heap,
    thread: &mut Thread,
    require: &RequireInfo,
    results: &[RawValue],
) -> Exec<()> {
    let exports = builtins::normalize_require_exports(results.first().copied());
    if heap
        .module_cache_set(&require.instance, require.epoch, exports)
        .is_none()
    {
        heap.module_load_end(&require.loading_key);
        heap.unpin(&require.module_pin);
        return Err(err("out of memory caching a required module"));
    }
    heap.module_load_end(&require.loading_key);
    heap.unpin(&require.module_pin);
    thread.top = require.saved_top;
    place_results(
        heap,
        thread,
        require.result_base,
        require.result_count,
        &[exports],
    )?;
    clear_call_temps(thread, require.result_base, 1, require.cleanup_end);
    Ok(())
}
