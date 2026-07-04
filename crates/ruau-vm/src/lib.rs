//! The Ruau VM: a pure-Rust sandboxed executor for Luau bytecode.
//!
//! The VM loads compiler-produced [`ruau_bytecode::BytecodeChunk`] values and
//! runs each instance single-threaded. It provides bytecode loading, execution,
//! limits, cancellation, standard libraries, sandboxing, runtime capabilities,
//! snapshots, host functions, scoped values, userdata, and marshaling.
//!
//! # Embedding shape
//!
//! A host first builds a [`Vm`] with [`Vm::builder`], spelling out the ambient
//! environment, default [`Limits`], [`RuntimeCapabilities`], and sandbox policy.
//! It then compiles source with a capability-aware compiler path, loads a
//! [`CompiledModule`] or [`ruau_bytecode::BytecodeChunk`], and runs it through
//! one of the owned-result entry points such as [`Vm::exec_async`]. Retained
//! VMs can unload modules and clear per-run state with [`Vm::clear_app_data`],
//! [`Vm::clear_named_registry`], and [`Vm::clear_module_cache`] before reuse.
//!
//! # Scope-branded values
//!
//! [`Scope`] is the borrowed lane where host code may inspect and construct Lua
//! values. [`ScopedValue`] is valid only for the scope step that produced it.
//! [`ruau_vm_api::OwnedValue`] is a low-level VM-owned callback payload for the
//! native module ABI. [`MarshaledValue`] is the durable copy used for results,
//! storage, JSON conversion, and cross-step boundaries. Use [`Stashed`] handles
//! when a host must retain a Lua value inside the same VM after the current
//! scope step returns.
//!
//! # Sync, async, and local execution
//!
//! The VM itself is not `Send` while executing. Async entry points drive host
//! futures cooperatively and honor wall-clock deadlines while parked. Native
//! embedders that need to drive an async VM entry point without a multi-threaded
//! runtime can use [`LocalExecutor`] or [`run_local`]; wasm embedders should
//! await the async entry point directly from their host loop. Synchronous scope
//! steps remain useful for host-side setup and retained callback calls that do
//! not need to await.
//!
//! Most users reach this API as `ruau::vm`. Depend on `ruau-vm` directly only
//! for VM-only embedding or runtime tooling.

// Test and conformance-harness builds expose helpers that make crate-private
// types pub-reachable; the lint's value is guarding the real embedder surface.
#![cfg_attr(any(test, feature = "conformance"), allow(unnameable_types))]
// `impl Vm` is split across `lib.rs` and `sandbox.rs`.
#![allow(clippy::multiple_inherent_impl)]

use std::{any::Any, borrow::Cow};

mod builder;
mod builtins;
mod call;
mod cancel;
mod conformance;
mod coroutine;
mod datetime;
mod debug;
mod driver;
mod execute;
mod features;
mod fingerprint;
mod func;
mod gas_profile;
mod gc;
mod hash;
mod heap;
mod host;
mod host_ext;
mod host_type;
mod limits;
mod load;
#[cfg(not(target_arch = "wasm32"))]
mod local;
mod object;
mod pack;
mod pattern;
mod registry;
mod runtime_capabilities;
mod runtime_compile;
mod sandbox;
mod scope;
pub mod serde;
mod snapshot;
mod state;
mod string;
mod table;
mod tm;
mod value_marshal;
mod vmutils;

#[cfg(any())]
pub(crate) use builder::test_vm;
pub use builder::{VmBuildError, VmBuilder, VmSandboxPolicy};
pub use cancel::{Cancel, CancellationToken};
pub use conformance::conformance_scope_revision;
#[cfg(any(test, feature = "conformance"))]
pub use conformance::{
    CONFORMANCE_ERRORS_MAX_CALL_DEPTH, CONFORMANCE_GAS, CONFORMANCE_PCALL_GAS,
    CONFORMANCE_PCALL_MAX_CALL_DEPTH, CONFORMANCE_TABLES_SPARSE_BOUNDARY_GAS,
    CONFORMANCE_WALL_SECS, ConformanceScopeDisposition, ConformanceScopeEntry,
    ConformanceScopeResult, ConformanceScriptConfig, ConformanceScriptOrigin,
    conformance_compile_options_for_script, conformance_config_for_script,
    conformance_config_for_script_source, conformance_features_for_script,
    conformance_limits_for_script, conformance_module_source,
    conformance_runtime_compilation_for_script, conformance_scope_entries,
    enable_luau_integer_type,
};
pub use debug::{ChunkName, ChunkNameKind, ChunkNameRef, SourceLocation, TracebackFrame};
pub use features::ExecutionFeatures;
pub use fingerprint::{SEMANTICS_REVISION, semantics_fingerprint};
pub use gas_profile::{GasProfile, GasProfileEntry};
#[cfg(any(test, feature = "conformance"))]
pub use heap::Heap;
use heap::Heap as VmHeap;
pub use host::{
    AsyncHostContext, AsyncHostFunction, HostScriptError, ScopedHostFunction, async_host_fn,
    async_module_host_callable, scoped_host_fn, scoped_module_host_callable,
};
pub use host_ext::{FromHostArgs, HostArgsError, IntoHostReturn, ModuleBuilderExt};
// Embedder-typed host userdata (`host_type` module): the build-time type
// descriptors plus the scope-branded borrow guards.
pub use host_type::{HostType, HostTypeBuilder};
// One canonical home per item: the embedder-facing configuration family
// (everything in `Vm`/`VmBuilder` signatures) lives at the crate root; callers
// name supported host-call ABI specifics from `ruau_vm_api`.
pub use limits::{Ambient, AmbientConfig, AmbientMode, GcPolicy};
pub use limits::{Deadline, Limits, SinkQuota};
pub use load::{CompiledModule, LoadError, LoadMode, LoadedModule};
#[cfg(not(target_arch = "wasm32"))]
pub use local::{LocalExecutor, run_local};
pub use registry::ModuleInstallError;
use ruau_bytecode::{BytecodeChunk, CompileOptions, compile_source};
#[cfg(feature = "derive")]
pub use ruau_embed_derive::{FromLua, IntoLua};
#[cfg(any())]
pub(crate) use ruau_source::InMemorySource;
#[cfg(any())]
pub(crate) use ruau_source::SyncModuleSource;
/// Source resolution for `require`: runtime `require` consumes the
/// async-first [`ruau_source::ModuleSource`] model. The async driver suspends
/// on pending resolve/read futures, then resumes to compile and run the
/// required module body. Synchronous entry points fail closed when they
/// encounter a pending source operation; callers that install async sources
/// must use the async VM entry points.
pub(crate) use ruau_source::{
    InstanceKey, ModuleId, ModuleSource, ModuleSourceError, ModuleSourceFuture, ModuleSourceResult,
    ReadRequest,
};
pub(crate) use ruau_vm_api::HeapId;
use ruau_vm_api::{HostPayload, RawValue, RuntimeErrorKind};
pub use runtime_capabilities::{Library, RuntimeCapabilities};
pub use runtime_compile::{RuntimeCompileContext, RuntimeCompileLimits, RuntimeCompiler};
pub use sandbox::SandboxError;
pub use scope::{
    Buffer, ContextMut, FromLua, FromLuaMulti, Function, FunctionId, FunctionInfo, IntoLua,
    IntoLuaMulti, IntoStash, KeyHandle, MethodArgs, MultiValue, RuntimeError, Scope, ScopedValue,
    ScriptError, Stashed, Str, Table, ThreadHandle, Userdata, UserdataRef, UserdataRefMut,
};
pub use snapshot::{MAX_SNAPSHOT_BYTES, SnapshotError, VmSnapshot};

/// VM-specific marker kinds for [`Stashed`] handles.
///
/// Typed stash handles use the canonical ABI marker kinds in [`ruau_vm_api::marker`].
/// This module keeps only the engine-level [`marker::Value`] kind for the
/// any-kind value stash ([`Scope::stash_value`](scope::Scope::stash_value)).
pub mod marker {
    /// A stashed value of any kind — the marker behind
    /// [`Scope::stash_value`](crate::Scope::stash_value) /
    /// [`Scope::fetch_value`](crate::Scope::fetch_value). Unlike the typed kinds
    /// it promises nothing about what the slot holds; `fetch_value` returns
    /// whatever [`ScopedValue`](crate::ScopedValue) kind was stashed.
    #[derive(Clone, Copy, Debug)]
    pub struct Value;
}

/// A function handle stashed past a [`Scope`] step.
pub type StashedClosure = Stashed<ruau_vm_api::marker::Closure>;

/// A table handle stashed past a [`Scope`] step.
pub type StashedTable = Stashed<ruau_vm_api::marker::Table>;

/// A value of any kind stashed past a [`Scope`] step
/// (see [`Scope::stash_value`]).
pub type StashedValue = Stashed<marker::Value>;

#[cfg(any(test, feature = "conformance"))]
pub use string::InternedString;
use table::LuaTable as VmLuaTable;
#[cfg(any(test, feature = "conformance"))]
pub use table::{LuaTable, NextStep};
pub use value_marshal::{DEFAULT_MAX_VALUE_MARSHAL_DEPTH, MarshaledPair, MarshaledValue};
pub(crate) use value_marshal::{ValueMarshalError, ValueMarshalLimits, ValueVisitor};

/// Host-provided sink for formatted `print` output.
pub type PrintSink = Box<dyn FnMut(&[u8]) + Send>;

/// Per-call context for VM invocation entry points.
///
/// Empty options inherit the VM's builder-level/default limits, print sink, and
/// app data. Setting a print sink or app data replaces that VM-level context
/// only for the call and restores the previous context when the call returns,
/// whether it succeeds, raises a catchable script error, or exits through fatal
/// control flow such as cancellation or deadline.
#[derive(Default)]
pub struct CallOptions {
    limits: Option<Limits>,
    print_sink: Option<PrintSink>,
    app_data: Option<scope::AppData>,
    cancel: Option<Cancel>,
}

impl CallOptions {
    /// Empty options: inherit the VM defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies per-invocation resource ceilings, overlaid on the VM defaults.
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Installs a per-call cancellation signal.
    ///
    /// This is a convenience for setting [`Limits::cancel`] in the per-call
    /// override. If both are supplied, this explicit call-context cancellation
    /// signal wins.
    #[must_use]
    pub fn cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Installs a per-call `print` sink.
    #[must_use]
    pub fn print_sink(mut self, sink: PrintSink) -> Self {
        self.print_sink = Some(sink);
        self
    }

    /// Installs a per-call `print` sink bounded by `quota`.
    #[must_use]
    pub fn print_sink_with_quota(mut self, sink: PrintSink, quota: SinkQuota) -> Self {
        self.print_sink = Some(quota.apply(sink));
        self
    }

    /// Installs typed app data visible to scoped host functions during this call.
    ///
    /// Per-call app data replaces the VM-level app-data map for the duration of
    /// the call, then the previous map is restored exactly.
    #[must_use]
    pub fn app_data<T: std::any::Any + Send + Sync>(mut self, value: T) -> Self {
        let app_data = self.app_data.get_or_insert_with(scope::AppData::default);
        app_data.set(value);
        self
    }

    /// Installs erased typed app data visible to scoped host functions during
    /// this call.
    ///
    /// The concrete type inside `value` is still the lookup key used by
    /// `Scope::app_data::<T>`; this form is for higher-level option builders
    /// that need to store app data before they know the final call context.
    #[must_use]
    pub fn app_data_erased(mut self, value: Box<dyn Any + Send + Sync>) -> Self {
        let app_data = self.app_data.get_or_insert_with(scope::AppData::default);
        app_data.set_boxed(value);
        self
    }

    fn effective_limits(&self, defaults: &Limits) -> Limits {
        let mut limits = match &self.limits {
            Some(overrides) => defaults.overlay(overrides),
            None => defaults.clone(),
        };
        if let Some(cancel) = &self.cancel {
            limits.cancel = Some(cancel.clone());
        }
        limits
    }
}

/// Error returned by [`Vm::step_result`] and [`Vm::step_with_result`].
///
/// The VM distinguishes failures raised while entering or managing the scoped
/// step from the host-defined error type returned by the step body. This keeps a
/// body closure ergonomic (`?` can return its own error) without flattening VM
/// poison or re-entry failures into that host error type.
///
/// Errors returned by the body are always [`Body`](StepError::Body), even when
/// the body's error type is [`RuntimeError`]. Use [`step`](Vm::step) or
/// [`step_with`](Vm::step_with) when nested VM runtime errors should remain the
/// direct error channel.
#[derive(Clone, Debug)]
pub enum StepError<E> {
    /// The step machinery failed before or around the body.
    Runtime(RuntimeError),
    /// The body closure returned its own error.
    Body(E),
}

impl<E> StepError<E> {
    /// The runtime error, when this failure came from the VM.
    #[must_use]
    pub fn runtime_error(&self) -> Option<&RuntimeError> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::Body(_) => None,
        }
    }

    /// The body error, when this failure came from the host closure.
    #[must_use]
    pub fn body_error(&self) -> Option<&E> {
        match self {
            Self::Runtime(_) => None,
            Self::Body(error) => Some(error),
        }
    }

    /// Unwraps a body error, returning the VM error when the failure came from
    /// the step machinery instead.
    pub fn into_body_error(self) -> Result<E, RuntimeError> {
        match self {
            Self::Runtime(error) => Err(error),
            Self::Body(error) => Ok(error),
        }
    }

    /// Maps the body error while preserving runtime failures unchanged.
    pub fn map_body_error<F, O>(self, f: impl FnOnce(E) -> O) -> StepError<O> {
        match self {
            Self::Runtime(error) => StepError::Runtime(error),
            Self::Body(error) => StepError::Body(f(error)),
        }
    }
}

impl<E> From<RuntimeError> for StepError<E> {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl<E: std::fmt::Display> std::fmt::Display for StepError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(error) => write!(f, "scope step failed: {error}"),
            Self::Body(error) => write!(f, "scope step body failed: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for StepError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::Body(error) => Some(error),
        }
    }
}

const SCRIPT_ERROR_TRACEBACK_MAX_BYTES: usize = 16 * 1024;

/// Version information for the VM crate.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Draws the next process-unique heap nonce. Starts above the small fixed ids the
/// unit tests build raw heaps with, so a counter-drawn VM nonce never collides
/// with them.
fn next_heap_id() -> HeapId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1 << 32);
    HeapId(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Which generational collection path ran.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionKind {
    /// Young-generation collection.
    Minor,
    /// Full-heap collection.
    Major,
}

/// Result of a collection request, explicit ([`Vm::collect`]) or routine
/// ([`Vm::collect_routine`]); every variant names the cycle kind the
/// collector chose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionOutcome {
    /// A collection cycle completed.
    Completed {
        /// Whether the cycle was minor or major.
        kind: CollectionKind,
        /// Number of heap objects reclaimed by the completed cycle.
        reclaimed: usize,
    },
    /// The VM is poisoned, so collecting would inspect an untrusted heap state.
    SkippedPoisoned,
    /// The main thread is currently taken out or otherwise unavailable as a root.
    SkippedMainThreadUnavailable,
    /// The collector abandoned the cycle before sweeping, usually because it could not
    /// grow its mark work list under memory pressure.
    Aborted {
        /// Whether the abandoned cycle started as a minor or major cycle.
        kind: CollectionKind,
    },
}

/// Result of a host-paced GC step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionStepOutcome {
    /// The step budget accumulated but did not yet trigger a collection cycle.
    Pending,
    /// A collection was requested or the step budget reached a cycle boundary.
    Collection(CollectionOutcome),
}

impl CollectionStepOutcome {
    /// Number of objects reclaimed, or zero while pending/skipped/aborted.
    #[must_use]
    pub fn reclaimed(self) -> usize {
        match self {
            Self::Pending => 0,
            Self::Collection(outcome) => outcome.reclaimed(),
        }
    }

    /// Whether a collection cycle completed.
    #[must_use]
    pub fn completed(self) -> bool {
        matches!(self, Self::Collection(outcome) if outcome.completed())
    }
}

impl CollectionOutcome {
    /// Number of objects reclaimed, or zero for skipped/aborted outcomes.
    #[must_use]
    pub fn reclaimed(self) -> usize {
        match self {
            Self::Completed { reclaimed, .. } => reclaimed,
            Self::SkippedPoisoned | Self::SkippedMainThreadUnavailable | Self::Aborted { .. } => 0,
        }
    }

    /// Whether a collection cycle completed, even if it reclaimed nothing.
    #[must_use]
    pub fn completed(self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

/// A single VM instance: it owns its heap and global state and is driven by one
/// `&mut self` loop.
pub struct Vm {
    heap: VmHeap,
    /// Host-initiated invocations armed on this VM. Build-time setup such as
    /// the trusted prelude and preload instantiation does not count.
    execution_count: u64,
    /// The live main thread, an arena object so the collector can trace it.
    /// The interpreter takes it out of the arena to run (restoring the disjoint
    /// register/heap borrow) and puts it back, like a coroutine resume.
    main_thread: ruau_vm_api::RawGc<ruau_vm_api::marker::Thread>,
    ambient: Ambient,
    limits: Limits,
    runtime_capabilities: RuntimeCapabilities,
    /// Set when a host-boundary call caught a panic. The heap/thread may be
    /// inconsistent, so every further entry point refuses to run — the host must
    /// drop this VM (the worker-restart contract, §8.5).
    poisoned: bool,
    poison_reason: Option<String>,
    /// The build-time hidden tables and support values, kept so
    /// [`Vm::clear_named_registry`] can re-register the host surface after
    /// wiping per-run named state.
    named_bindings: Vec<registry::NamedBinding>,
    /// Typed host app data, in its own cell (not the heap's) so a `Scope` read does
    /// not collide with value construction. Interior-mutable so a step's `&Scope`
    /// can borrow it while the step holds the heap borrow.
    app_data: std::cell::RefCell<scope::AppData>,
    /// Modules instantiated at build time from [`VmBuilder::preload`] artifacts,
    /// in registration order, awaiting [`Vm::take_preloaded`].
    preloaded: Vec<LoadedModule>,
}

/// A heap-consistency violation found by [`Vm::validate`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeapValidationError {
    detail: String,
}

impl HeapValidationError {
    /// Diagnostic description of the first violation found.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for HeapValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "heap validation failed: {}", self.detail)
    }
}

impl std::error::Error for HeapValidationError {}

/// Error installing a shared basic-type metatable: the handle did not resolve
/// to a live table in this VM.
///
/// Public only alongside its producers, the conformance-gated
/// [`Vm::set_vector_metatable`] and raw [`Heap`] setup hooks; the always-built
/// string-metatable setup uses it internally.
#[cfg(any(test, feature = "conformance"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetatableNotResident;

/// Error installing a shared basic-type metatable: the handle did not resolve
/// to a live table in this VM.
#[allow(clippy::cfg_not_test)] // production visibility; test/conformance builds use the `pub` type above
#[cfg(not(any(test, feature = "conformance")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MetatableNotResident;

impl std::fmt::Display for MetatableNotResident {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("metatable handle does not resolve to a live table in this VM")
    }
}

impl std::error::Error for MetatableNotResident {}

/// Error returned while binding a loaded chunk to a fresh per-chunk environment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindChunkEnvironmentError {
    /// The VM has no main-thread global table to use as the environment fallback.
    MissingGlobals,
    /// Allocating the environment table, metatable, or `__index` key failed.
    OutOfMemory,
    /// The loaded module's main closure is not resident in this VM.
    ModuleNotResident,
}

impl std::fmt::Display for BindChunkEnvironmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingGlobals => f.write_str("VM has no global table"),
            Self::OutOfMemory => f.write_str("allocation failed while binding chunk environment"),
            Self::ModuleNotResident => f.write_str("loaded module is not resident in this VM"),
        }
    }
}

impl std::error::Error for BindChunkEnvironmentError {}

/// A catchable script failure from a VM-level protected call.
#[derive(Clone, Debug)]
pub struct ProtectedScriptError {
    value: RawValue,
    kind: RuntimeErrorKind,
    traceback: Option<String>,
    frames: Vec<TracebackFrame>,
    frames_truncated: bool,
    payload: Option<HostPayload>,
}

impl ProtectedScriptError {
    fn from_failure(
        heap: &mut VmHeap,
        failure: call::ProtectedFailure,
        capture: Option<debug::Traceback>,
    ) -> Self {
        let kind = failure.error.kind;
        let in_flight = failure.error.host_payload.clone();
        // Attach the structured frames only when the stashed capture belongs to
        // this failure: its rendered text must match the text the failure
        // carries, so a stale stash (from a failure another surface consumed,
        // or from a boundary that captured no traceback) is never inherited.
        let (frames, frames_truncated) =
            debug::frames_for_traceback(failure.traceback.as_deref(), capture);
        let value = call::materialize(heap, failure.error);
        let payload = call::recover_host_payload(heap, in_flight, value);
        Self {
            value,
            kind,
            traceback: failure.traceback,
            frames,
            frames_truncated,
            payload,
        }
    }

    /// The materialized Lua error value.
    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub fn value(&self) -> RawValue {
        self.value
    }

    /// The failure category carried to runner metrics.
    #[must_use]
    pub fn kind(&self) -> RuntimeErrorKind {
        self.kind
    }

    /// The captured traceback, if available.
    ///
    /// Tracebacks and error locations are captured by the engine, not by the
    /// `debug` library: they are identical whether or not the VM's
    /// [`RuntimeCapabilities`] installs [`Library::Debug`], which gates script-visible
    /// introspection only.
    #[must_use]
    pub fn traceback(&self) -> Option<&str> {
        self.traceback.as_deref()
    }

    /// The structured frames of the captured traceback, innermost first — the
    /// data the rendered [`traceback`](Self::traceback) text is derived from,
    /// so an embedder can map frames to its own location types without parsing
    /// the text. Empty when no traceback was captured.
    ///
    /// Frames honor the same byte budget as the text: a frame is collected
    /// only when its fully rendered line fits the remaining budget. When the
    /// budget cuts the text short mid-frame, the partially rendered frame is
    /// dropped from this list and [`frames_truncated`](Self::frames_truncated)
    /// reports the cut.
    #[must_use]
    pub fn frames(&self) -> &[TracebackFrame] {
        &self.frames
    }

    /// The innermost source-located frame for this script failure, if one was
    /// captured.
    #[must_use]
    pub fn primary_frame(&self) -> Option<&TracebackFrame> {
        debug::primary_user_frame(&self.frames)
    }

    /// Whether the traceback byte budget cut frame collection short: the
    /// rendered [`traceback`](Self::traceback) text ends in a truncated line,
    /// and the frame it belonged to (with any frames past it) is absent from
    /// [`frames`](Self::frames).
    #[must_use]
    pub fn frames_truncated(&self) -> bool {
        self.frames_truncated
    }

    /// The typed host payload riding the caught error, if the error was raised
    /// by a host function via [`scope::RuntimeError::with_payload`] (directly,
    /// or re-raised by the script as the same error value). See that method
    /// for the preservation/loss semantics.
    #[must_use]
    pub fn payload_ref<T: std::any::Any>(&self) -> Option<&T> {
        self.payload.as_ref().and_then(HostPayload::downcast_ref)
    }
}

/// A catchable script failure whose value has been copied out of the VM.
///
/// `PartialEq` compares the marshaled value, kind, and traceback (text and
/// frames); an attached host payload is compared by identity (the same shared
/// payload).
#[derive(Clone, Debug, PartialEq)]
pub struct MarshaledScriptError {
    value: MarshaledValue,
    kind: RuntimeErrorKind,
    traceback: Option<String>,
    frames: Vec<TracebackFrame>,
    frames_truncated: bool,
    payload: Option<HostPayload>,
}

impl MarshaledScriptError {
    fn new(
        value: MarshaledValue,
        kind: RuntimeErrorKind,
        traceback: Option<String>,
        frames: Vec<TracebackFrame>,
        frames_truncated: bool,
        payload: Option<HostPayload>,
    ) -> Self {
        Self {
            value,
            kind,
            traceback,
            frames,
            frames_truncated,
            payload,
        }
    }

    /// The owned Lua error value surfaced by the protected entry call.
    #[must_use]
    pub fn value(&self) -> &MarshaledValue {
        &self.value
    }

    /// The failure category carried to runner metrics.
    #[must_use]
    pub fn kind(&self) -> RuntimeErrorKind {
        self.kind
    }

    /// The captured traceback, if available.
    ///
    /// As with [`ProtectedScriptError::traceback`], the capture is
    /// engine-owned and unaffected by profiling out [`Library::Debug`].
    #[must_use]
    pub fn traceback(&self) -> Option<&str> {
        self.traceback.as_deref()
    }

    /// The structured frames of the captured traceback, innermost first,
    /// carried over from the in-VM [`ProtectedScriptError`]; see
    /// [`ProtectedScriptError::frames`] for the derivation and byte-budget
    /// semantics.
    #[must_use]
    pub fn frames(&self) -> &[TracebackFrame] {
        &self.frames
    }

    /// The innermost source-located frame for this script failure, if one was
    /// captured.
    #[must_use]
    pub fn primary_frame(&self) -> Option<&TracebackFrame> {
        debug::primary_user_frame(&self.frames)
    }

    /// Whether the traceback byte budget cut frame collection short; see
    /// [`ProtectedScriptError::frames_truncated`].
    #[must_use]
    pub fn frames_truncated(&self) -> bool {
        self.frames_truncated
    }

    /// The typed host payload riding the caught error, carried over from the
    /// in-VM [`ProtectedScriptError`]; see
    /// [`scope::RuntimeError::with_payload`] for the preservation/loss
    /// semantics.
    ///
    /// The payload is host-only freight beside the marshaled value, not part
    /// of it: [`value`](Self::value) never renders it (there is no
    /// `MarshaledValue::Opaque` entanglement), and serializing the marshaled
    /// error does not serialize the payload.
    #[must_use]
    pub fn payload_ref<T: std::any::Any>(&self) -> Option<&T> {
        self.payload.as_ref().and_then(HostPayload::downcast_ref)
    }

    /// A conservative display message for the owned Lua error value.
    ///
    /// String errors return their bytes lossily decoded as UTF-8. Scalar values
    /// use Luau's scalar spelling. Tables and other heap-derived values return
    /// their Luau type name.
    #[must_use]
    pub fn message(&self) -> String {
        self.value.display_lua()
    }
}

/// Flattened error for owned protected entry points.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecError {
    /// The script raised a catchable error; its value and traceback were copied
    /// out of the VM.
    Script(MarshaledScriptError),
    /// The call observed its cancellation signal.
    Cancelled,
    /// The call exceeded a wall-clock deadline.
    Deadline,
    /// The VM was already poisoned or this call poisoned it.
    PanicPoison,
    /// Copying a success value or script error value into owned form failed.
    Marshal {
        /// Path-aware marshal failure text.
        message: String,
    },
}

impl ExecError {
    fn from_marshal_error(error: &ValueMarshalError) -> Self {
        Self::Marshal {
            message: marshal_error_message(error),
        }
    }

    /// The failure category carried to runner metrics.
    #[must_use]
    pub fn kind(&self) -> RuntimeErrorKind {
        match self {
            Self::Script(error) => error.kind(),
            Self::Cancelled => RuntimeErrorKind::Cancelled,
            Self::Deadline => RuntimeErrorKind::Deadline,
            Self::PanicPoison => RuntimeErrorKind::PanicPoison,
            Self::Marshal { .. } => RuntimeErrorKind::Runtime,
        }
    }

    /// The catchable script error, if this is [`ExecError::Script`].
    #[must_use]
    pub fn script_error(&self) -> Option<&MarshaledScriptError> {
        match self {
            Self::Script(error) => Some(error),
            _ => None,
        }
    }

    /// A conservative display message for this flattened error.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Script(error) => error.message(),
            Self::Cancelled => "cancelled".to_owned(),
            Self::Deadline => "deadline exceeded".to_owned(),
            Self::PanicPoison => "VM is poisoned".to_owned(),
            Self::Marshal { message } => message.clone(),
        }
    }
}

/// Common read-only metadata exposed by VM error surfaces.
///
/// The trait deliberately keeps value access out of the common contract because
/// each error surface owns a different value shape: [`ScriptError`] is
/// scope-branded, [`HostScriptError`] owns [`ruau_vm_api::OwnedValue`],
/// [`MarshaledScriptError`] owns [`MarshaledValue`], and fatal runtime errors
/// have no script value at all. Use each type's `value()` method when the value
/// matters.
pub trait VmErrorInfo {
    /// The failure category carried to runner metrics.
    fn kind(&self) -> RuntimeErrorKind;

    /// The captured traceback text, if this surface has one.
    fn traceback(&self) -> Option<&str> {
        None
    }

    /// Structured traceback frames, innermost first.
    fn frames(&self) -> &[TracebackFrame] {
        &[]
    }

    /// The innermost source-located frame, if one was captured.
    fn primary_frame(&self) -> Option<&TracebackFrame> {
        debug::primary_user_frame(self.frames())
    }

    /// Whether the traceback byte budget cut frame collection short.
    fn frames_truncated(&self) -> bool {
        false
    }

    /// Typed host freight attached to this error, if any.
    fn payload_ref<T: Any>(&self) -> Option<&T> {
        None
    }

    /// A display-ready message when this surface can produce one without extra
    /// context.
    fn display_message(&self) -> Option<Cow<'_, str>> {
        None
    }
}

impl VmErrorInfo for RuntimeError {
    fn kind(&self) -> RuntimeErrorKind {
        Self::kind(self)
    }

    fn payload_ref<T: Any>(&self) -> Option<&T> {
        Self::payload_ref(self)
    }

    fn display_message(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Borrowed(Self::message(self)))
    }
}

impl VmErrorInfo for ProtectedScriptError {
    fn kind(&self) -> RuntimeErrorKind {
        Self::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        Self::traceback(self)
    }

    fn frames(&self) -> &[TracebackFrame] {
        Self::frames(self)
    }

    fn frames_truncated(&self) -> bool {
        Self::frames_truncated(self)
    }

    fn payload_ref<T: Any>(&self) -> Option<&T> {
        Self::payload_ref(self)
    }
}

impl VmErrorInfo for MarshaledScriptError {
    fn kind(&self) -> RuntimeErrorKind {
        Self::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        Self::traceback(self)
    }

    fn frames(&self) -> &[TracebackFrame] {
        Self::frames(self)
    }

    fn frames_truncated(&self) -> bool {
        Self::frames_truncated(self)
    }

    fn payload_ref<T: Any>(&self) -> Option<&T> {
        Self::payload_ref(self)
    }

    fn display_message(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Owned(Self::message(self)))
    }
}

impl VmErrorInfo for ExecError {
    fn kind(&self) -> RuntimeErrorKind {
        Self::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        self.script_error()
            .and_then(MarshaledScriptError::traceback)
    }

    fn frames(&self) -> &[TracebackFrame] {
        self.script_error()
            .map_or(&[], MarshaledScriptError::frames)
    }

    fn frames_truncated(&self) -> bool {
        self.script_error()
            .is_some_and(MarshaledScriptError::frames_truncated)
    }

    fn payload_ref<T: Any>(&self) -> Option<&T> {
        self.script_error()
            .and_then(MarshaledScriptError::payload_ref)
    }

    fn display_message(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Owned(Self::message(self)))
    }
}

impl VmErrorInfo for HostScriptError {
    fn kind(&self) -> RuntimeErrorKind {
        Self::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        Self::traceback(self)
    }

    fn frames(&self) -> &[TracebackFrame] {
        Self::frames(self)
    }

    fn frames_truncated(&self) -> bool {
        Self::frames_truncated(self)
    }

    fn display_message(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Owned(Self::message(self)))
    }
}

impl VmErrorInfo for ScriptError<'_> {
    fn kind(&self) -> RuntimeErrorKind {
        ScriptError::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        ScriptError::traceback(self)
    }

    fn frames(&self) -> &[TracebackFrame] {
        ScriptError::frames(self)
    }

    fn frames_truncated(&self) -> bool {
        ScriptError::frames_truncated(self)
    }

    fn payload_ref<T: Any>(&self) -> Option<&T> {
        ScriptError::payload_ref(self)
    }
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Script(error) => write!(f, "script error ({:?})", error.kind()),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Deadline => f.write_str("deadline exceeded"),
            Self::PanicPoison => f.write_str("VM is poisoned"),
            Self::Marshal { message } => write!(f, "owned result marshal failed: {message}"),
        }
    }
}

impl std::error::Error for ExecError {}

fn marshal_error_message(error: &ValueMarshalError) -> String {
    format!("owned entry-point result marshal failed at {error}")
}

struct CallContextRestore {
    app_data: Option<scope::AppData>,
    print_sink: Option<Option<PrintSink>>,
}

pin_project_lite::pin_project! {
    struct CatchUnwindFuture<F> {
        #[pin]
        future: F,
    }
}

impl<F> std::future::Future for CatchUnwindFuture<F>
where
    F: std::future::Future,
{
    type Output = std::thread::Result<F::Output>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let future = self.project().future;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.poll(cx))) {
            Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(payload) => std::task::Poll::Ready(Err(payload)),
        }
    }
}

fn catch_unwind_future<F>(future: F) -> CatchUnwindFuture<F>
where
    F: std::future::Future,
{
    CatchUnwindFuture { future }
}

impl Vm {
    /// Starts building a VM.
    #[must_use]
    pub fn builder() -> VmBuilder {
        VmBuilder::default()
    }

    /// The owning heap.
    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub fn heap(&self) -> &VmHeap {
        &self.heap
    }

    /// Number of host-initiated invocations armed on this VM.
    ///
    /// The counter is monotonic for the VM's lifetime and increments when an
    /// invocation receives its per-call limits, before sync/async dispatch or a
    /// [`step_with`](Self::step_with) body runs. Loading bytecode,
    /// trusted build-time setup, and poisoned-entry refusals do not increment it.
    #[must_use]
    pub const fn execution_count(&self) -> u64 {
        self.execution_count
    }

    /// Whether a borrowed [`Scope`] is currently active on this VM lane.
    #[must_use]
    pub fn is_scope_active(&self) -> bool {
        self.heap.scope_active()
    }

    /// Gas attribution for the most recently completed profiled invocation.
    ///
    /// Returns `None` until an invocation runs with [`Limits::gas_profile`]
    /// enabled, and is cleared again at the start of the next invocation.
    #[must_use]
    pub fn gas_profile(&self) -> Option<&GasProfile> {
        self.heap.gas_profile()
    }

    /// The limits inherited by calls whose [`CallOptions`] do not override them.
    #[must_use]
    pub const fn default_limits(&self) -> &Limits {
        &self.limits
    }

    /// Replaces the limits inherited by subsequent calls.
    ///
    /// Use this on retained VMs after trusted setup when the steady-state entry
    /// budget differs from the builder-time setup budget.
    pub fn set_default_limits(&mut self, limits: Limits) {
        self.limits = limits;
        self.apply_default_limits();
    }

    /// Bytes currently charged against this VM's heap.
    #[must_use]
    pub fn heap_used_bytes(&self) -> usize {
        self.heap.used_bytes()
    }

    /// Highest charged heap byte total observed by this VM.
    #[must_use]
    pub fn peak_heap_bytes(&self) -> usize {
        self.heap.peak_bytes()
    }

    /// Completed garbage-collection cycles over this VM's lifetime.
    #[must_use]
    pub fn gc_cycles(&self) -> u64 {
        self.heap.gc_cycles()
    }

    /// Gas units spent by the current or most recently completed invocation.
    #[must_use]
    pub fn gas_spent(&self) -> u64 {
        self.heap.gas_spent()
    }

    /// The live main thread — for host inspection between calls. Returns `None` when
    /// the VM is poisoned, or while a `call`/`call_async` has taken the thread out of
    /// the arena to run (that window is not otherwise observable, since the call
    /// holds `&mut self`). The filter on `id` distinguishes the live thread from the
    /// default placeholder the take-out leaves behind.
    #[must_use]
    #[cfg(any())]
    pub(crate) fn main_thread(&self) -> Option<&state::Thread> {
        if self.poisoned {
            return None;
        }
        self.heap
            .thread(self.main_thread)
            .filter(|thread| thread.id == Some(self.main_thread))
    }

    /// The main thread's global table, when one is installed — the narrow
    /// public read embedder tooling needs (the thread itself is internal).
    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub fn globals(&self) -> Option<ruau_vm_api::RawGc<ruau_vm_api::marker::Table>> {
        self.heap
            .thread(self.main_thread)
            .and_then(|thread| thread.globals)
    }

    /// Runs a full stop-the-world collection, rooting the resident main thread.
    /// Returns a [`CollectionOutcome`] so hosts can distinguish "completed but
    /// reclaimed nothing" from "skipped" or "aborted". Collection is skipped when
    /// the VM is poisoned or the main thread is not resident (taken out by a
    /// running call leaves an `id`-less placeholder): an empty root set would not
    /// over-retain, it would *under-mark and sweep the live heap*, so collection is
    /// skipped rather than run blind.
    ///
    /// **Precondition — no externally-held bare handles across a collect.** This roots
    /// the main thread, the heap's own roots (string metatable), and every `registry`
    /// pin. A [`LoadedModule`] from [`Vm::load`] is rooted (it pins its main closure), so
    /// holding one across a collection is safe — but a bare handle copied *out* of it
    /// (`module.main`, or a `RawValue` returned by a call) is not, and goes stale after
    /// an intervening `collect`. The same applies to a host function that stashes a
    /// `RawValue` from its arguments across a collection. Do not retain a bare
    /// handle across a `collect`; use registry pins or owned values instead.
    /// Script-driven `collectgarbage("collect")` runs the same collection at the
    /// next root dispatch safepoint; see `Heap::request_gc`.
    pub fn collect(&mut self) -> CollectionOutcome {
        if self.poisoned {
            return CollectionOutcome::SkippedPoisoned;
        }
        let resident = self
            .heap
            .thread(self.main_thread)
            .is_some_and(|thread| thread.id == Some(self.main_thread));
        if !resident {
            return CollectionOutcome::SkippedMainThreadUnavailable;
        }
        // The explicit collect API promises full reclamation, so force a major (a minor
        // leaves old garbage for a later major).
        self.heap.gc_force_major = true;
        match gc::collect(
            &mut self.heap,
            &[gc::GcRef::Thread(self.main_thread.index())],
        ) {
            Some(reclaimed) => CollectionOutcome::Completed {
                kind: CollectionKind::Major,
                reclaimed,
            },
            None => CollectionOutcome::Aborted {
                kind: CollectionKind::Major,
            },
        }
    }

    /// Runs the routine generational collector decision used by dispatch
    /// safepoints and reports whether it chose a minor or major cycle.
    ///
    /// This is intentionally separate from [`Vm::collect`]: explicit host
    /// collection promises full reclamation and therefore forces a major cycle,
    /// while benchmark and service-observability code needs to track the normal
    /// allocation-paced minor path without reaching into private collector
    /// internals.
    pub fn collect_routine(&mut self) -> CollectionOutcome {
        if self.poisoned {
            return CollectionOutcome::SkippedPoisoned;
        }
        let resident = self
            .heap
            .thread(self.main_thread)
            .is_some_and(|thread| thread.id == Some(self.main_thread));
        if !resident {
            return CollectionOutcome::SkippedMainThreadUnavailable;
        }
        let kind = if self.heap.gc_should_major() {
            CollectionKind::Major
        } else {
            CollectionKind::Minor
        };
        match gc::collect(
            &mut self.heap,
            &[gc::GcRef::Thread(self.main_thread.index())],
        ) {
            Some(reclaimed) => CollectionOutcome::Completed { kind, reclaimed },
            None => CollectionOutcome::Aborted { kind },
        }
    }

    /// Advances the manual GC-step accumulator and, once it reaches a cycle
    /// boundary, runs at most one routine collection immediately.
    ///
    /// This is the host-facing counterpart to Luau's `collectgarbage("step",
    /// units)`: a fleet scheduler can call it on different VMs between ticks so
    /// threshold collections are paced by the host instead of clumping at the
    /// next script dispatch safepoint. The collection path is the same guarded
    /// routine path as [`collect_routine`](Self::collect_routine): poisoned VMs
    /// and VMs with a taken-out main thread report a skipped outcome rather
    /// than collecting from an unsafe root set.
    ///
    /// A pending script-side `collectgarbage("collect")`/`"step"` request is
    /// also serviced here, so hosts that pace GC explicitly do not leave a
    /// duplicate request for the next dispatch.
    pub fn collect_step(&mut self, units: usize) -> CollectionStepOutcome {
        let due = self.heap.request_host_gc_step(units);
        let requested = self.heap.take_gc_request();
        if !(due || requested) {
            return CollectionStepOutcome::Pending;
        }
        CollectionStepOutcome::Collection(self.collect_routine())
    }

    /// Checks the heap's GC consistency: every handle held by a live object — and every
    /// root — resolves to a live arena slot. A dangling handle means the collector freed
    /// a still-referenced object (a latent use-after-free). Read-only and non-recursive;
    /// intended to run after a collection and as the GC-stress gate's invariant check.
    ///
    /// # Errors
    /// Returns the first dangling handle found, with a diagnostic description.
    pub fn validate(&self) -> Result<(), HeapValidationError> {
        // The VM-owned `main_thread` root lives outside the heap's own root set, so check
        // it (generation-checked) before delegating to the arena/root walk.
        if self.heap.thread(self.main_thread).is_none() {
            return Err(HeapValidationError {
                detail: "dangling main_thread root".to_owned(),
            });
        }
        gc::validate(&self.heap).map_err(|detail| HeapValidationError { detail })
    }

    /// Captures this quiescent VM's heap into opaque snapshot bytes.
    ///
    /// Snapshots are intentionally narrow: the VM must be deterministic and idle, and
    /// prototype support refuses host userdata, host functions, host types, and runtime
    /// module sources. Restore into a freshly built compatible VM with
    /// [`Vm::restore_snapshot`]. Encoded snapshots are capped at
    /// [`MAX_SNAPSHOT_BYTES`].
    ///
    /// # Errors
    /// Returns [`SnapshotError`] when the VM is not at a supported snapshot point or
    /// when encoding fails.
    pub fn snapshot(&mut self) -> Result<VmSnapshot, SnapshotError> {
        if self.poisoned {
            return Err(SnapshotError::NotQuiescent("VM is poisoned"));
        }
        let stamp = snapshot::SnapshotStamp::from_vm(self);
        let heap = self.heap.snapshot_image()?;
        snapshot::encode_envelope(&snapshot::new_envelope(stamp, self.main_thread, heap))
    }

    /// Restores `snapshot` into this compatible template VM.
    ///
    /// The template supplies host setup, ambient configuration, limits, runtime
    /// capabilities, and registry shape. The snapshot supplies heap state and the main thread. Treat
    /// stored snapshot bytes as untrusted input unless your host storage layer
    /// authenticates them; restore checks the fixed header and semantic fingerprint
    /// before decoding the heap body.
    ///
    /// # Errors
    /// Returns [`SnapshotError`] when the snapshot bytes are invalid, the template is
    /// incompatible, or the restored heap fails validation.
    pub fn restore_snapshot(self, snapshot: &VmSnapshot) -> Result<Self, SnapshotError> {
        snapshot::restore_snapshot_bytes(self, snapshot.as_bytes())
    }

    /// The ambient mode this VM was built with.
    #[must_use]
    pub fn ambient(&self) -> Ambient {
        self.ambient
    }

    /// The resource ceilings.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The runtime capabilities this VM was built with.
    #[must_use]
    pub fn runtime_capabilities(&self) -> &RuntimeCapabilities {
        &self.runtime_capabilities
    }

    /// Loads a compiled chunk into a runnable module, validating untrusted
    /// bytecode. Use [`Vm::load_with`] to choose the load mode.
    ///
    /// # Errors
    /// Returns a [`LoadError`] for a malformed, unsupported, or compile-error
    /// chunk.
    pub fn load(&mut self, chunk: &BytecodeChunk) -> Result<LoadedModule, LoadError> {
        self.load_with(chunk, LoadMode::Validated)
    }

    /// Loads a compiled chunk under an explicit [`LoadMode`] — `Trusted` skips
    /// structural verification for bytecode the process compiled itself.
    ///
    /// # Errors
    /// Returns a [`LoadError`] as for [`Vm::load`].
    pub fn load_with(
        &mut self,
        chunk: &BytecodeChunk,
        mode: LoadMode,
    ) -> Result<LoadedModule, LoadError> {
        let module = load::load_with_limits(
            &mut self.heap,
            chunk,
            mode,
            load::DEFAULT_CHUNK_NAME,
            self.limits.effective(),
        )?;
        self.bind_module_environment(&module);
        Ok(module)
    }

    /// Loads a compiled chunk under an explicit chunk name, which its prototypes
    /// report in runtime-error locations and `debug` queries. The name carries a
    /// `luaO_chunkid` marker — `=name`/`@name` display as `name`, a bare string as
    /// `[string "…"]`. Use [`ChunkName`] to construct or inspect these bytes
    /// without hand-formatting markers.
    ///
    /// # Errors
    /// Returns a [`LoadError`] as for [`Vm::load`].
    pub fn load_named(
        &mut self,
        chunk: &BytecodeChunk,
        chunk_name: &[u8],
    ) -> Result<LoadedModule, LoadError> {
        let module = load::load_with_limits(
            &mut self.heap,
            chunk,
            LoadMode::Validated,
            chunk_name,
            self.limits.effective(),
        )?;
        self.bind_module_environment(&module);
        Ok(module)
    }

    /// Loads bytecode produced by repository-owned upstream fixture or
    /// conformance tooling.
    ///
    /// This is not a public compatibility path: ordinary embedding APIs accept
    /// only [`ruau_bytecode::DEFAULT_VERSION`].
    #[doc(hidden)]
    #[cfg(any(test, feature = "conformance"))]
    pub fn load_upstream_fixture(
        &mut self,
        chunk: &BytecodeChunk,
    ) -> Result<LoadedModule, LoadError> {
        self.load_upstream_fixture_named(chunk, load::DEFAULT_CHUNK_NAME)
    }

    /// Loads upstream fixture bytecode under an explicit chunk name.
    #[doc(hidden)]
    #[cfg(any(test, feature = "conformance"))]
    pub fn load_upstream_fixture_named(
        &mut self,
        chunk: &BytecodeChunk,
        chunk_name: &[u8],
    ) -> Result<LoadedModule, LoadError> {
        let module = load::load_upstream_fixture_named_with_limits(
            &mut self.heap,
            chunk,
            LoadMode::Validated,
            chunk_name,
            self.limits.effective(),
        )?;
        self.bind_module_environment(&module);
        Ok(module)
    }

    /// Instantiates a [`CompiledModule`] artifact into this VM: the
    /// compile-once, instantiate-many path. The artifact's chunk was validated
    /// when the artifact was built and is immutable (host-constructed, behind
    /// an `Arc`, with no mutating surface), so this load skips both
    /// recompilation and structural re-verification — its cost is proportional
    /// to instantiation alone: building this VM's proto graph, interning the
    /// constant strings into this VM's heap, and allocating the main closure.
    /// The loader still range-checks every reference it resolves, so a bad
    /// artifact yields a [`LoadError`], never a panic.
    ///
    /// The artifact's bytes stay host-owned and are charged to no VM; the
    /// per-VM instantiation (protos, strings, closure) is charged against this
    /// VM's memory cap exactly as [`Vm::load`] charges it.
    ///
    /// Fails closed when the artifact's [`RuntimeCapabilities`] is not
    /// identical to this VM's: a chunk compiled under a different capability
    /// surface (different constant-fold and import suppression) must never run
    /// here.
    ///
    /// # Errors
    /// Returns [`LoadError::RuntimeCapabilitiesMismatch`] for a capability
    /// mismatch, or a [`LoadError`] as for [`Vm::load`].
    pub fn load_compiled(&mut self, module: &CompiledModule) -> Result<LoadedModule, LoadError> {
        if module.runtime_capabilities() != &self.runtime_capabilities {
            return Err(LoadError::RuntimeCapabilitiesMismatch {
                artifact: module.runtime_capabilities().clone(),
                vm: self.runtime_capabilities.clone(),
            });
        }
        self.load_with(module.chunk(), LoadMode::Trusted)
    }

    /// Takes ownership of the modules instantiated at build time by
    /// [`VmBuilder::preload`], in registration order. Subsequent calls return
    /// an empty vector. Run and release them like any loaded module.
    pub fn take_preloaded(&mut self) -> Vec<LoadedModule> {
        std::mem::take(&mut self.preloaded)
    }

    /// Loads a compiled chunk as the body for a concrete module id.
    ///
    /// Runtime `require` uses this id as the requester for relative imports from
    /// the entry chunk, matching module bodies loaded by the VM's
    /// [`ModuleSource`] resolver.
    ///
    /// # Errors
    /// Returns a [`LoadError`] as for [`Vm::load`].
    pub fn load_module(
        &mut self,
        chunk: &BytecodeChunk,
        module_id: ModuleId,
    ) -> Result<LoadedModule, LoadError> {
        let module = load::load_module_with_limits(
            &mut self.heap,
            chunk,
            LoadMode::Validated,
            module_id,
            self.limits.effective(),
        )?;
        self.bind_module_environment(&module);
        Ok(module)
    }

    /// Loads a compiled chunk as the body for a concrete module id while using
    /// an explicit chunk name for tracebacks and debug locations.
    ///
    /// Use this when a higher-level source model has separate runtime requester
    /// identity and human-facing load identity. [`Vm::load_module`] remains the
    /// simpler lower-level form when the module id is also the desired chunk
    /// name.
    ///
    /// # Errors
    /// Returns a [`LoadError`] as for [`Vm::load`].
    pub fn load_named_module(
        &mut self,
        chunk: &BytecodeChunk,
        module_id: ModuleId,
        chunk_name: &[u8],
    ) -> Result<LoadedModule, LoadError> {
        let module = load::load_named_module_with_limits(
            &mut self.heap,
            chunk,
            LoadMode::Validated,
            module_id,
            chunk_name,
            self.limits.effective(),
        )?;
        self.bind_module_environment(&module);
        Ok(module)
    }

    /// Releases a [`LoadedModule`]'s registry pin, making its main closure — and the
    /// prototype graph and source string reachable only through it — collectable
    /// again. Consumes the module so it cannot be called or unloaded twice. Dropping a
    /// module without unloading leaks its pin until the VM is dropped (the `luaL_unref`
    /// model).
    #[allow(
        clippy::needless_pass_by_value,
        reason = "consumes the handle to enforce single-use: a module cannot be unloaded or called again"
    )]
    pub fn unload(&mut self, module: LoadedModule) {
        self.heap.unpin(&module.pin);
    }

    /// Installs (or, with `None`, clears) the shared `vector` metatable used by
    /// the upstream conformance harness.
    ///
    /// This raw-handle setup hook is not part of the ordinary embedder surface.
    ///
    /// # Errors
    /// Returns an error if `metatable` is `Some` of a handle that does not
    /// resolve to a live table in this VM.
    #[cfg(any(test, feature = "conformance"))]
    pub fn set_vector_metatable(
        &mut self,
        metatable: Option<ruau_vm_api::RawGc<ruau_vm_api::marker::Table>>,
    ) -> Result<(), MetatableNotResident> {
        self.heap.set_vector_metatable(metatable)
    }

    /// Installs the harness-only `getcoverage` global used by upstream's
    /// conformance runner. This is not part of any production profile.
    ///
    /// # Errors
    /// Returns an error if the VM has no global table or allocation fails.
    #[cfg(any(test, feature = "conformance"))]
    pub fn install_conformance_coverage_helper(&mut self) -> Result<(), String> {
        let globals = self
            .heap
            .thread(self.main_thread)
            .and_then(|thread| thread.globals)
            .ok_or_else(|| "VM has no global table".to_owned())?;
        let closure = self
            .heap
            .alloc_builtin(builtins::Builtin::ConformanceGetCoverage)
            .ok_or_else(|| "out of memory installing getcoverage".to_owned())?;
        set_member(
            &mut self.heap,
            globals,
            b"getcoverage",
            RawValue::Function(closure),
        )
        .ok_or_else(|| "out of memory installing getcoverage".to_owned())
    }

    /// Installs the harness-only `resumeerror` global used by upstream's
    /// conformance runner. This is not part of any production profile.
    ///
    /// # Errors
    /// Returns an error if the VM has no global table or allocation fails.
    #[cfg(any(test, feature = "conformance"))]
    pub fn install_conformance_resume_error_helper(&mut self) -> Result<(), String> {
        let globals = self
            .heap
            .thread(self.main_thread)
            .and_then(|thread| thread.globals)
            .ok_or_else(|| "VM has no global table".to_owned())?;
        let closure = self
            .heap
            .alloc_builtin(builtins::Builtin::ConformanceResumeError)
            .ok_or_else(|| "out of memory installing resumeerror".to_owned())?;
        set_member(
            &mut self.heap,
            globals,
            b"resumeerror",
            RawValue::Function(closure),
        )
        .ok_or_else(|| "out of memory installing resumeerror".to_owned())
    }

    /// Installs the harness-only `setblockallocations` global used by upstream's
    /// conformance runner. This is not part of any production profile.
    ///
    /// # Errors
    /// Returns an error if the VM has no global table or allocation fails.
    #[cfg(any(test, feature = "conformance"))]
    pub fn install_conformance_block_allocations_helper(&mut self) -> Result<(), String> {
        let globals = self
            .heap
            .thread(self.main_thread)
            .and_then(|thread| thread.globals)
            .ok_or_else(|| "VM has no global table".to_owned())?;
        let closure = self
            .heap
            .alloc_builtin(builtins::Builtin::ConformanceSetBlockAllocations)
            .ok_or_else(|| "out of memory installing setblockallocations".to_owned())?;
        set_member(
            &mut self.heap,
            globals,
            b"setblockallocations",
            RawValue::Function(closure),
        )
        .ok_or_else(|| "out of memory installing setblockallocations".to_owned())
    }

    /// Installs the first harness-only native-yield helpers for `cyield.luau`.
    ///
    /// These globals model upstream C continuation helpers. They are deliberately
    /// conformance-only and are not part of the production host ABI.
    ///
    /// # Errors
    /// Returns an error if the VM has no global table or allocation fails.
    #[cfg(any(test, feature = "conformance"))]
    pub fn install_conformance_yield_helpers(&mut self) -> Result<(), String> {
        let globals = self
            .heap
            .thread(self.main_thread)
            .and_then(|thread| thread.globals)
            .ok_or_else(|| "VM has no global table".to_owned())?;
        for (name, builtin) in [
            (
                b"singleYield".as_slice(),
                builtins::Builtin::ConformanceSingleYield,
            ),
            (
                b"multipleYields".as_slice(),
                builtins::Builtin::ConformanceMultipleYields,
            ),
            (
                b"multipleYieldsWithNestedCall".as_slice(),
                builtins::Builtin::ConformanceMultipleYieldsWithNestedCall,
            ),
            (
                b"passthroughCall".as_slice(),
                builtins::Builtin::ConformancePassthroughCall,
            ),
            (
                b"passthroughCallMoreResults".as_slice(),
                builtins::Builtin::ConformancePassthroughCallMoreResults,
            ),
            (
                b"passthroughCallArgReuse".as_slice(),
                builtins::Builtin::ConformancePassthroughCallArgReuse,
            ),
            (
                b"passthroughCallVaradic".as_slice(),
                builtins::Builtin::ConformancePassthroughCallVaradic,
            ),
            (
                b"passthroughCallWithState".as_slice(),
                builtins::Builtin::ConformancePassthroughCallWithState,
            ),
        ] {
            let closure = self.heap.alloc_builtin(builtin).ok_or_else(|| {
                format!("out of memory installing {}", String::from_utf8_lossy(name))
            })?;
            set_member(&mut self.heap, globals, name, RawValue::Function(closure)).ok_or_else(
                || format!("out of memory installing {}", String::from_utf8_lossy(name)),
            )?;
        }
        Ok(())
    }

    /// Installs feature-gated `getfenv`/`setfenv` compatibility for conformance scripts
    /// that explicitly opt into [`ExecutionFeatures::fenv`]. Conformance-only:
    /// not part of the production embedding surface.
    ///
    /// # Errors
    /// Returns an error if the VM has no global table or allocation fails.
    #[cfg(any(test, feature = "conformance"))]
    pub fn install_fenv_compat_helpers(&mut self) -> Result<(), String> {
        let globals = self
            .heap
            .thread(self.main_thread)
            .and_then(|thread| thread.globals)
            .ok_or_else(|| "VM has no global table".to_owned())?;
        let closure = self
            .heap
            .alloc_builtin(builtins::Builtin::CompatGetFenv)
            .ok_or_else(|| "out of memory installing getfenv".to_owned())?;
        set_member(
            &mut self.heap,
            globals,
            b"getfenv",
            RawValue::Function(closure),
        )
        .ok_or_else(|| "out of memory installing getfenv".to_owned())?;
        let closure = self
            .heap
            .alloc_builtin(builtins::Builtin::CompatSetFenv)
            .ok_or_else(|| "out of memory installing setfenv".to_owned())?;
        set_member(
            &mut self.heap,
            globals,
            b"setfenv",
            RawValue::Function(closure),
        )
        .ok_or_else(|| "out of memory installing setfenv".to_owned())
    }

    fn bind_module_environment(&mut self, module: &LoadedModule) {
        let globals = self
            .heap
            .thread(self.main_thread)
            .and_then(|thread| thread.globals);
        if let Some(closure) = self.heap.closure_mut(module.main) {
            closure.env = globals;
        }
    }

    /// Binds `module`'s main closure to a **fresh per-chunk environment** rather
    /// than directly to globals: a new table whose metatable's `__index` falls
    /// through to the VM globals. The chunk therefore *reads* the global surface,
    /// but its top-level assignments land in the per-chunk table and do not mutate
    /// shared globals — the per-exec environment a host wraps each execution in
    /// (distinct from the crate-internal `bind_module_environment`).
    /// The per-exec `require` overlay lands with runtime `require`.
    ///
    /// # Errors
    /// Returns a structured error if the VM has no globals table, if allocation
    /// fails, or if `module` is not resident in this VM. The module's binding is
    /// left unchanged on failure.
    pub fn bind_chunk_environment(
        &mut self,
        module: &LoadedModule,
    ) -> Result<(), BindChunkEnvironmentError> {
        let globals = self
            .heap
            .thread(self.main_thread)
            .and_then(|t| t.globals)
            .ok_or(BindChunkEnvironmentError::MissingGlobals)?;
        // Build the environment and its `{ __index = globals }` metatable, then
        // publish by binding the closure. No dispatch safepoint runs on this
        // synchronous path and `alloc_table`/`intern_str` do not collect inline, so
        // the interim tables cannot be swept before they are reachable and need no
        // rooting; a future alloc-triggered collection would change that.
        let env = self
            .heap
            .alloc_table(table::LuaTable::new())
            .ok_or(BindChunkEnvironmentError::OutOfMemory)?;
        let metatable = self
            .heap
            .alloc_table(table::LuaTable::new())
            .ok_or(BindChunkEnvironmentError::OutOfMemory)?;
        let index_key = self
            .heap
            .intern_str(b"__index")
            .ok_or(BindChunkEnvironmentError::OutOfMemory)?;
        self.heap
            .table_mut(metatable)
            .ok_or(BindChunkEnvironmentError::ModuleNotResident)?
            .set(RawValue::String(index_key), RawValue::Table(globals));
        self.heap
            .table_mut(env)
            .ok_or(BindChunkEnvironmentError::ModuleNotResident)?
            .set_metatable(Some(metatable));
        self.heap
            .closure_mut(module.main)
            .ok_or(BindChunkEnvironmentError::ModuleNotResident)?
            .env = Some(env);
        Ok(())
    }

    /// Installs an async host function under `binding` (test scaffolding for
    /// the async-host install path; embedders go through `ModuleBuilder`).
    ///
    /// # Errors
    /// Returns a message when allocation fails.
    #[cfg(any())]
    pub(crate) fn install_async_host_function(
        &mut self,
        name: &'static str,
        binding: ruau_vm_api::ModuleBinding,
        f: Box<dyn AsyncHostFunction>,
    ) -> Result<(), String> {
        // Rooting note: like bind_chunk_environment, this synchronous setup path
        // relies on alloc_async_host, intern_str, and alloc_table not collecting
        // inline. The closure and any fresh library table are rooted only when
        // the final table.set publishes them. If allocation starts collecting
        // inline, root interim values before later allocations.
        let closure = self
            .heap
            .alloc_async_host(f)
            .ok_or_else(|| format!("out of memory installing async host function {name}"))?;
        self.install_function_value(name, binding, RawValue::Function(closure))
    }

    #[cfg(any())]
    fn install_function_value(
        &mut self,
        name: &'static str,
        binding: ruau_vm_api::ModuleBinding,
        value: RawValue,
    ) -> Result<(), String> {
        let key = RawValue::String(
            self.heap
                .intern_str(name.as_bytes())
                .ok_or_else(|| format!("out of memory interning host function name {name}"))?,
        );
        let target = self.binding_target(binding)?;
        let existing = self
            .heap
            .table(target)
            .ok_or_else(|| format!("host binding target for {name} is missing"))?
            .get(key);
        if !matches!(existing, RawValue::Nil) {
            return Err(format!("host binding {name} already exists"));
        }
        let table = self
            .heap
            .table_mut(target)
            .ok_or_else(|| format!("host binding target for {name} is missing"))?;
        if table.set(key, value) {
            Ok(())
        } else {
            Err(format!(
                "host function name {name} is not a valid table key"
            ))
        }
    }
    #[cfg(any())]
    fn binding_target(
        &mut self,
        binding: ruau_vm_api::ModuleBinding,
    ) -> Result<ruau_vm_api::RawGc<ruau_vm_api::marker::Table>, String> {
        let globals = self
            .heap
            .thread(self.main_thread)
            .and_then(|t| t.globals)
            .ok_or_else(|| "VM has no globals table".to_owned())?;
        match binding {
            ruau_vm_api::ModuleBinding::Global | ruau_vm_api::ModuleBinding::GlobalOverride => {
                Ok(globals)
            }
            ruau_vm_api::ModuleBinding::Library(name) => self.library_table(globals, name.as_ref()),
            ruau_vm_api::ModuleBinding::Hidden(name) => Err(format!(
                "hidden binding `{name}` is not supported by this test helper"
            )),
        }
    }
    #[cfg(any())]
    fn library_table(
        &mut self,
        globals: ruau_vm_api::RawGc<ruau_vm_api::marker::Table>,
        name: &str,
    ) -> Result<ruau_vm_api::RawGc<ruau_vm_api::marker::Table>, String> {
        let key = RawValue::String(
            self.heap
                .intern_str(name.as_bytes())
                .ok_or_else(|| format!("out of memory interning host library name {name}"))?,
        );
        let existing = self
            .heap
            .table(globals)
            .ok_or_else(|| "VM globals table is missing".to_owned())?
            .get(key);
        match existing {
            RawValue::Table(existing) => Ok(existing),
            RawValue::Nil => {
                let library = self
                    .heap
                    .alloc_table(VmLuaTable::new())
                    .ok_or_else(|| format!("out of memory allocating host library {name}"))?;
                if self
                    .heap
                    .table_mut(globals)
                    .ok_or_else(|| "VM globals table is missing".to_owned())?
                    .set(key, RawValue::Table(library))
                {
                    Ok(library)
                } else {
                    Err(format!("host library name {name} is not a valid table key"))
                }
            }
            _ => Err(format!(
                "host binding target {name} already exists and is not a table"
            )),
        }
    }

    /// Runs one borrowed-[`Scope`] lane step: `f` receives a `&Scope` over the
    /// heap, builds and persists values, and returns an [`IntoStash`] result that
    /// outlives the step. No scope-borrowed handle can escape — the closure is
    /// higher-ranked (`for<'s>`), so its return type cannot name the scope brand,
    /// and the [`IntoStash`] bound additionally rejects the raw, unbranded handles
    /// that the brand alone would miss.
    ///
    /// The re-entry guard forbids opening a nested step while one is active on this
    /// lane (a host re-entering the VM mid-step); the guard is released even if `f`
    /// panics, via the scope's `Drop`.
    ///
    /// # Example
    /// Build a value and persist it past the step:
    /// ```text
    /// let stashed = vm.step(|s| {
    ///     let table = s.create_table()?;
    ///     s.stash_table(table)
    /// });
    /// ```
    ///
    /// A scope-borrowed handle cannot escape the step. Returning one does not
    /// compile because the step's return type cannot name the scope brand `'s`:
    /// ```text
    /// let escaped = vm.step(|s| s.create_table());
    /// ```
    ///
    /// Nor a type that *indirectly* contains a handle:
    /// ```text
    /// let escaped = vm.step(|s| Ok(vec![s.create_table()?]));
    /// ```
    ///
    /// Nor a raw, unbranded handle: `RawValue` is `'static`, so the brand alone
    /// would miss it, but it is not [`IntoStash`]:
    /// ```text
    /// let escaped = vm.step(|_s| Ok(RawValue::Nil));
    /// ```
    ///
    /// Compile-fail coverage for these contracts lives under
    /// `crates/ruau-vm/tests/ui/`.
    ///
    /// # Errors
    /// Returns the [`scope::RuntimeError`] the step body produced, or a re-entry error if a
    /// scope step is already active.
    pub fn step<R: scope::IntoStash>(
        &mut self,
        f: impl for<'s> FnOnce(&scope::Scope<'s>) -> Result<R, scope::RuntimeError>,
    ) -> Result<R, scope::RuntimeError> {
        // A poisoned VM refuses every entry point (like `call`/`call_async`): its
        // heap may be mid-mutation after a contained panic, so it must be dropped,
        // not stepped.
        if self.poisoned {
            return Err(scope::RuntimeError::poisoned());
        }
        if !self.heap.try_enter_scope() {
            return Err(scope::RuntimeError::runtime(
                "cannot open a nested scope step while one is active on this lane",
            ));
        }
        // Release any `Stashed` whose last clone dropped off-lane since the previous
        // step, before the step observes the registry.
        self.heap.drain_releases();
        // Take the main thread out (the same take-out/put-back as `contained`), so a
        // `Scope::call` has the disjoint `&mut Heap` + `&mut Thread` a nested Luau
        // call needs. The thread is owned here across `catch_unwind`, so its arena
        // slot is restored even on a caught panic.
        let main_id = self.main_thread;
        let Some(mut thread) = self.heap.take_thread(main_id) else {
            self.heap.exit_scope();
            self.poisoned = true;
            return Err(scope::RuntimeError::poisoned());
        };
        let _host_app_data = self.heap.enter_host_app_data(&self.app_data);
        let outcome = {
            let scope = scope::Scope::new(&mut self.heap, &mut thread, &self.app_data);
            // The step body may run Luau via `Scope::call`, so a Rust panic mid-call
            // can leave the heap torn: contain it and poison rather than unwind
            // through the live `&mut Heap`. A Lua *error* is already a clean `Result`
            // from `run_function`. `Scope`'s `Drop` clears the re-entry guard
            // whichever way the body exits.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&scope)))
        };
        if !self.heap.put_thread(main_id, thread) {
            // The main thread's slot is its own GC root for the whole run, so this
            // is unreachable; poison rather than continue with a hollow main thread.
            self.poisoned = true;
        }
        match outcome {
            Ok(result) => result,
            Err(_) => {
                self.poisoned = true;
                Err(scope::RuntimeError::poisoned())
            }
        }
    }

    /// Runs one borrowed-[`Scope`] lane step whose body returns its own error
    /// type.
    ///
    /// This is the host-error-friendly sibling of [`step`](Self::step). The
    /// successful value must still satisfy [`IntoStash`], so raw or
    /// scope-borrowed VM handles cannot escape the step. A [`RuntimeError`]
    /// raised by the step machinery is returned as [`StepError::Runtime`], while
    /// an error returned by `f` is returned as [`StepError::Body`].
    ///
    /// # Errors
    /// Returns [`StepError::Runtime`] for step-entry failures (including poison
    /// or re-entry), and [`StepError::Body`] for the closure's own error.
    pub fn step_result<R: scope::IntoStash, E>(
        &mut self,
        f: impl for<'s> FnOnce(&scope::Scope<'s>) -> Result<R, E>,
    ) -> Result<R, StepError<E>> {
        let mut body = None;
        self.step(|scope| {
            body = Some(f(scope));
            Ok(())
        })
        .map_err(StepError::Runtime)?;
        match body.expect("scope step body must run before step returns") {
            Ok(value) => Ok(value),
            Err(error) => Err(StepError::Body(error)),
        }
    }

    /// Runs one borrowed-[`Scope`] lane step under per-invocation resource
    /// ceilings, mirroring a module invocation: the
    /// override is overlaid on the builder defaults, the spent-gas counter is
    /// reset, and the defaults are restored after the step.
    ///
    /// Plain [`step`](Self::step) arms nothing — Luau run through its scope
    /// (`Scope::call`, `Scope::call_protected`) draws on whatever budget the
    /// previous `call*` invocation left behind. A host that *executes* scripts
    /// from a step (invoking a stashed callback between runs on a long-lived
    /// VM) should use this entry so every invocation gets a fresh budget and
    /// can carry its own `Cancel`/gas override.
    ///
    /// # Errors
    /// As [`step`](Self::step): the [`scope::RuntimeError`] the step body
    /// produced (including gas exhaustion or cancellation surfaced by a nested
    /// call), or a re-entry error if a scope step is already active.
    pub fn step_with<R: scope::IntoStash>(
        &mut self,
        options: &CallOptions,
        f: impl for<'s> FnOnce(&scope::Scope<'s>) -> Result<R, scope::RuntimeError>,
    ) -> Result<R, scope::RuntimeError> {
        if self.poisoned {
            return Err(scope::RuntimeError::poisoned());
        }
        let limits = options.effective_limits(&self.limits);
        self.begin_invocation(&limits);
        let outcome = self.step(f);
        if !self.poisoned {
            self.finish_invocation();
        }
        outcome
    }

    /// Runs one borrowed-[`Scope`] lane step under per-invocation resource
    /// ceilings while allowing the body to return its own error type.
    ///
    /// See [`step_result`](Self::step_result) for the error split, and
    /// [`step_with`](Self::step_with) for the per-step resource semantics.
    ///
    /// # Errors
    /// Returns [`StepError::Runtime`] for step-entry failures (including poison
    /// or re-entry), and [`StepError::Body`] for the closure's own error.
    pub fn step_with_result<R: scope::IntoStash, E>(
        &mut self,
        options: &CallOptions,
        f: impl for<'s> FnOnce(&scope::Scope<'s>) -> Result<R, E>,
    ) -> Result<R, StepError<E>> {
        let mut body = None;
        self.step_with(options, |scope| {
            body = Some(f(scope));
            Ok(())
        })
        .map_err(StepError::Runtime)?;
        match body.expect("scope step body must run before step returns") {
            Ok(value) => Ok(value),
            Err(error) => Err(StepError::Body(error)),
        }
    }

    /// Runs one borrowed-[`Scope`] lane step with a borrowed host context.
    ///
    /// The context is non-`Send`, borrowed rather than owned, and visible inside
    /// the step through [`Scope::context_mut`](crate::Scope::context_mut). Nested scoped host calls
    /// opened by Luau code in this step observe the same context.
    ///
    /// # Errors
    /// As [`step_with`](Self::step_with).
    pub fn step_with_context<T: Any, R: scope::IntoStash>(
        &mut self,
        context: &mut T,
        options: &CallOptions,
        f: impl for<'s> FnOnce(&scope::Scope<'s>) -> Result<R, scope::RuntimeError>,
    ) -> Result<R, scope::RuntimeError> {
        let context = scope::ContextSlot::new(context);
        let _host_context = self.heap.enter_host_context(&context);
        self.step_with(options, f)
    }

    /// Runs one borrowed-[`Scope`] lane step with a borrowed host context while
    /// allowing the body to return its own error type.
    ///
    /// # Errors
    /// Returns [`StepError::Runtime`] for step-entry failures (including poison
    /// or re-entry), and [`StepError::Body`] for the closure's own error.
    pub fn step_with_context_result<T: Any, R: scope::IntoStash, E>(
        &mut self,
        context: &mut T,
        options: &CallOptions,
        f: impl for<'s> FnOnce(&scope::Scope<'s>) -> Result<R, E>,
    ) -> Result<R, StepError<E>> {
        let context = scope::ContextSlot::new(context);
        let _host_context = self.heap.enter_host_context(&context);
        self.step_with_result(options, f)
    }

    /// Installs typed host state on this VM, readable from a [`Scope`]
    /// step via `Scope::app_data`/`app_data_mut`. One value per Rust type; replaces
    /// any previous value of the same type. This is host state — an untrusted script
    /// cannot reach it.
    pub fn set_app_data<T: Any + Send + Sync>(&mut self, value: T) {
        self.app_data.get_mut().set(value);
    }

    /// Removes the typed host state of type `T`, returning whether it was present.
    pub fn remove_app_data<T: Any>(&mut self) -> bool {
        self.app_data.get_mut().remove::<T>()
    }

    /// Drops all installed app data — the execution-session RAII cleanup a host runs
    /// at the end of an execution so per-run state never leaks into the next run on a
    /// pooled VM.
    pub fn clear_app_data(&mut self) {
        self.app_data.get_mut().clear();
    }

    /// Releases every named-registry entry (the host-side string-keyed state set
    /// through `Scope::named_set`), unpinning each value — the per-run cleanup
    /// counterpart to [`clear_app_data`](Vm::clear_app_data) for a pooled VM.
    ///
    /// Build-time named bindings (hidden module tables and support chunk
    /// returns) are part of the host surface, not per-run state: they are
    /// re-registered immediately, so the clear never strips them. Re-pinning
    /// a build-time binding cannot fail short of a registry allocation
    /// failure, which poisons the VM rather than leaving the surface partially
    /// installed.
    pub fn clear_named_registry(&mut self) {
        self.heap.clear_named();
        for binding in &self.named_bindings {
            if self.heap.named_set(&binding.name, binding.value).is_none() {
                self.poisoned = true;
                self.poison_reason.get_or_insert_with(|| {
                    "re-registering build-time named bindings failed".into()
                });
                return;
            }
        }
    }

    /// Releases every cached `require` module (its pinned exports) — the per-run
    /// reset a pooled host runs so one run's `package.loaded` does not leak into the
    /// next. A host that wants modules shared across runs simply omits this call.
    pub fn clear_module_cache(&mut self) {
        self.heap.clear_module_cache();
    }

    /// Installs a sink for `print`/log output: `print` formats its arguments and
    /// writes each line to `sink`. Without one, `print` discards (a no-op). The host
    /// owns and bounds the destination, so a script's print volume is the host's to
    /// cap — [`set_print_sink_with_quota`](Self::set_print_sink_with_quota) does the
    /// bounding for it. Replaces any previous sink.
    pub fn set_print_sink(&mut self, sink: PrintSink) {
        self.heap.set_print_sink(sink);
    }

    /// Installs a `print` sink bounded by `quota`: byte and call ceilings with a
    /// deterministic truncation marker, so a host running untrusted scripts does
    /// not hand-roll the accounting. See [`SinkQuota`] for the counting and
    /// truncation contract. Replaces any previous sink; installing afresh resets
    /// the quota, which is how a pooled host scopes quotas per run.
    pub fn set_print_sink_with_quota(&mut self, sink: PrintSink, quota: SinkQuota) {
        self.heap.set_print_sink(quota.apply(sink));
    }

    /// Runs a loaded module's main closure to completion synchronously.
    ///
    /// The synchronous path enforces gas and logical deadlines, but **not**
    /// wall-clock deadlines — `Deadline::Wall` is honored by the async
    /// driver's governed await. A synchronous embedder that needs a
    /// wall-clock bound should install [`Cancel::after`] (a detached
    /// watchdog thread) or its own externally-cancelled signal via
    /// `Limits::cancel`.
    ///
    /// # Errors
    /// Returns the [`ruau_vm_api::Unwind`] of an uncaught runtime error, or a contained
    /// panic (after which the VM is poisoned).
    #[cfg(any(test, feature = "conformance"))]
    pub fn call(
        &mut self,
        module: &LoadedModule,
        mut options: CallOptions,
    ) -> Result<Vec<RawValue>, ruau_vm_api::Unwind> {
        let limits = options.effective_limits(&self.limits);
        let restore = self.install_call_options(&mut options);
        let result = self.call_with_effective_limits(module, &limits);
        self.restore_call_options(restore);
        result
    }

    #[cfg(any(test, feature = "conformance"))]
    fn call_with_effective_limits(
        &mut self,
        module: &LoadedModule,
        limits: &Limits,
    ) -> Result<Vec<RawValue>, ruau_vm_api::Unwind> {
        if self.poisoned {
            return Err(Self::panic_poison_unwind());
        }
        self.begin_invocation(limits);
        let main = module.main;
        let result = self.contained(|heap, thread| call::run(heap, thread, main));
        if !self.poisoned {
            self.finish_invocation();
        }
        result
    }

    /// Runs a loaded module's main closure to completion synchronously in
    /// protected mode.
    ///
    /// A successful script returns `Ok(Ok(values))`. A catchable script error
    /// returns `Ok(Err(error))` with the Lua error value and pre-unwind
    /// traceback. Fatal setup failures, cancellation, or panic poison remain an
    /// outer `Err`, so a protected run cannot swallow them.
    #[cfg(any(test, feature = "conformance"))]
    pub fn call_protected(
        &mut self,
        module: &LoadedModule,
        mut options: CallOptions,
    ) -> Result<Result<Vec<RawValue>, ProtectedScriptError>, ruau_vm_api::Unwind> {
        let limits = options.effective_limits(&self.limits);
        let restore = self.install_call_options(&mut options);
        let result = self.call_protected_with_effective_limits(module, &limits);
        self.restore_call_options(restore);
        result
    }

    fn call_protected_with_effective_limits(
        &mut self,
        module: &LoadedModule,
        limits: &Limits,
    ) -> Result<Result<Vec<RawValue>, ProtectedScriptError>, ruau_vm_api::Unwind> {
        if self.poisoned {
            return Err(Self::panic_poison_unwind());
        }
        self.begin_invocation(limits);
        let main = module.main;
        let result = self.contained(|heap, thread| {
            match call::run_protected_with_traceback(
                heap,
                thread,
                main,
                SCRIPT_ERROR_TRACEBACK_MAX_BYTES,
            ) {
                Ok(Ok(values)) => Ok(Ok(values)),
                Ok(Err(failure)) if failure.error.is_catchable() => {
                    let capture = thread.captured_traceback.take();
                    Ok(Err(ProtectedScriptError::from_failure(
                        heap, failure, capture,
                    )))
                }
                Ok(Err(failure)) => {
                    let kind = failure.error.kind;
                    Err(ruau_vm_api::Unwind {
                        error: call::materialize(heap, failure.error),
                        kind,
                    })
                }
                Err(error) => {
                    let kind = error.kind;
                    Err(ruau_vm_api::Unwind {
                        error: call::materialize(heap, error),
                        kind,
                    })
                }
            }
        });
        if !self.poisoned {
            self.finish_invocation();
        }
        result
    }

    /// Loads and runs the trusted build-time [`PRELUDE`] into this VM, returning
    /// whether it succeeded. The chunk is process-compiled (so loaded `Trusted`)
    /// and runs against the freshly installed globals before the ceilings arm.
    fn run_prelude(&mut self) -> bool {
        let Ok(module) = self.load_with(prelude_chunk(), LoadMode::Trusted) else {
            return false;
        };
        let main = module.main;
        let ok = self
            .contained(|heap, thread| call::run(heap, thread, main))
            .is_ok();
        // The prelude's main closure is dead once it has installed the globals; unload
        // it so its registry pin does not linger for the VM's lifetime.
        self.unload(module);
        ok
    }

    fn run_support_chunks(&mut self, chunks: &[registry::SupportChunk]) -> Result<(), String> {
        for chunk in chunks {
            self.run_support_chunk(chunk)?;
        }
        Ok(())
    }

    fn run_support_chunk(&mut self, chunk: &registry::SupportChunk) -> Result<(), String> {
        let compiler = self.heap.runtime_compiler();
        let limits = RuntimeCompileLimits::from_effective(self.limits.effective());
        let bytecode = compiler
            .compile(&chunk.source, RuntimeCompileContext::new(limits, None))
            .map_err(|message| {
                format!(
                    "support chunk `{}` from module `{}` failed to compile: {}",
                    String::from_utf8_lossy(&chunk.key),
                    chunk.module,
                    String::from_utf8_lossy(&message)
                )
            })?;
        let chunk_name = format!(
            "=support:{}:{}",
            chunk.module,
            String::from_utf8_lossy(&chunk.key)
        );
        let loaded = self
            .load_named(&bytecode, chunk_name.as_bytes())
            .map_err(|error| {
                format!(
                    "support chunk `{}` from module `{}` failed to load: {error}",
                    String::from_utf8_lossy(&chunk.key),
                    chunk.module
                )
            })?;
        let main = loaded.main;
        let result = self.contained(|heap, thread| call::run(heap, thread, main));
        let values = match result {
            Ok(values) => values,
            Err(error) => {
                self.unload(loaded);
                return Err(format!(
                    "support chunk `{}` from module `{}` failed to run: {:?}",
                    String::from_utf8_lossy(&chunk.key),
                    chunk.module,
                    error.kind
                ));
            }
        };
        let [value] = values.as_slice() else {
            self.unload(loaded);
            return Err(format!(
                "support chunk `{}` from module `{}` must return exactly one value",
                String::from_utf8_lossy(&chunk.key),
                chunk.module
            ));
        };
        if self.heap.named_set(&chunk.key, *value).is_none() {
            self.unload(loaded);
            return Err(format!(
                "support chunk `{}` from module `{}` could not be rooted",
                String::from_utf8_lossy(&chunk.key),
                chunk.module
            ));
        }
        self.named_bindings.push(registry::NamedBinding {
            name: chunk.key.clone(),
            value: *value,
        });
        self.unload(loaded);
        Ok(())
    }

    /// Runs a loaded module on the async driver, awaiting any asynchronous host
    /// calls the script makes — the asynchronous analog of [`call`](Self::call).
    /// Runs on a tokio runtime; while a host future is pending the VM is not
    /// borrowed, so other VMs on the runtime make progress.
    ///
    /// # Errors
    /// Returns the [`ruau_vm_api::Unwind`] of an uncaught runtime error or a failed async
    /// host call.
    ///
    /// A parked host await is bounded by the request's wall-clock deadline and
    /// cancellation token (from [`Limits`]); either surfaces as a runtime error so
    /// a never-resolving host future cannot hold the worker. A panic in the driver —
    /// a host future, a VM bug, or materialization — poisons the VM (via the drop
    /// guard) so a caller that catches it at the task boundary cannot reuse a
    /// half-suspended VM, matching the synchronous `call`'s `catch_unwind` guard.
    #[cfg(any(test, feature = "conformance"))]
    pub async fn call_async(
        &mut self,
        module: &LoadedModule,
        mut options: CallOptions,
    ) -> Result<Vec<RawValue>, ruau_vm_api::Unwind> {
        let limits = options.effective_limits(&self.limits);
        let restore = self.install_call_options(&mut options);
        let result = self.call_async_with_effective_limits(module, &limits).await;
        self.restore_call_options(restore);
        result
    }

    /// Runs a loaded module on the async driver with per-invocation resource ceilings.
    ///
    /// The override applies only to this call; the VM's builder-level defaults
    /// remain in place for later invocations.
    ///
    /// # Errors
    /// Returns the [`ruau_vm_api::Unwind`] of an uncaught runtime error or a failed async
    /// host call.
    #[cfg(any(test, feature = "conformance"))]
    async fn call_async_with_effective_limits(
        &mut self,
        module: &LoadedModule,
        limits: &Limits,
    ) -> Result<Vec<RawValue>, ruau_vm_api::Unwind> {
        if self.poisoned {
            return Err(Self::panic_poison_unwind());
        }
        self.begin_invocation(limits);
        let main = module.main;
        // The wall-clock deadline and cancel token govern every parked host await
        // (the cancel token is also polled at the synchronous dispatch safepoint).
        // A `Deadline::Logical` is reserved for the deterministic model harness and
        // is not yet enforced — it maps to no wall-clock deadline here.
        let governance = driver::Governance {
            deadline: match limits.deadline {
                Some(Deadline::Wall(instant)) => Some(instant),
                _ => None,
            },
            cancel: limits.cancel.clone(),
        };
        // Pessimistically poison while the async run is in flight; cleared only after
        // the driver returns cleanly. The driver takes the main thread out only for
        // synchronous dispatch/resume segments and puts it back before every await or
        // preemptive yield, so a dropped pending future leaves a poisoned but resident
        // VM rather than a hollow main-thread slot.
        self.poisoned = true;
        let invocation = self.heap.begin_async_invocation();
        let main_id = self.main_thread;
        let outcome = catch_unwind_future(driver::run_async(
            &mut self.heap,
            main_id,
            main,
            &governance,
            &self.app_data,
        ))
        .await;
        self.finish_async_invocation(invocation, outcome)
    }

    /// Runs a loaded module on the async driver in protected mode.
    ///
    /// A successful script returns `Ok(Ok(values))`. A catchable script error
    /// returns `Ok(Err(error))` with the Lua error value and pre-unwind
    /// traceback. Fatal control flow such as cancellation, deadline, or panic
    /// poison remains an outer `Err`, so a protected run cannot swallow it.
    #[cfg(any(test, feature = "conformance"))]
    pub async fn call_protected_async(
        &mut self,
        module: &LoadedModule,
        mut options: CallOptions,
    ) -> Result<Result<Vec<RawValue>, ProtectedScriptError>, ruau_vm_api::Unwind> {
        let limits = options.effective_limits(&self.limits);
        let restore = self.install_call_options(&mut options);
        let result = self
            .call_protected_async_with_effective_limits(module, &limits)
            .await;
        self.restore_call_options(restore);
        result
    }

    /// Calls a Lua closure value with `args` on the async driver in protected mode.
    ///
    /// This is the suspendable counterpart to [`call_function`](Self::call_function):
    /// the target and raw arguments are trusted embedder values from this VM, and
    /// stale/dangling/cross-VM handles are rejected as an outer [`ruau_vm_api::Unwind`]
    /// before the protected script boundary opens. Catchable failures raised by
    /// the callee, including async host errors after an await, return as the inner
    /// [`ProtectedScriptError`].
    #[cfg(any())]
    pub(crate) async fn call_function_protected_async(
        &mut self,
        func: RawValue,
        args: &[RawValue],
    ) -> Result<Result<Vec<RawValue>, ProtectedScriptError>, ruau_vm_api::Unwind> {
        let limits = self.limits.clone();
        self.call_function_protected_async_with_limits(func, args, limits)
            .await
    }

    /// Calls a Lua closure value with `args` on the async driver in protected mode
    /// with per-invocation resource ceilings.
    #[cfg(any())]
    pub(crate) async fn call_function_protected_async_with_limits(
        &mut self,
        func: RawValue,
        args: &[RawValue],
        limits: Limits,
    ) -> Result<Result<Vec<RawValue>, ProtectedScriptError>, ruau_vm_api::Unwind> {
        if self.poisoned {
            return Err(Self::panic_poison_unwind());
        }
        if let Err(message) = call::validate_call_inputs(&self.heap, func, args) {
            return Err(self.runtime_unwind(message));
        }
        let limits = self.limits.overlay(&limits);
        self.begin_invocation(&limits);
        let governance = driver::Governance {
            deadline: match limits.deadline {
                Some(Deadline::Wall(instant)) => Some(instant),
                _ => None,
            },
            cancel: limits.cancel.clone(),
        };
        self.poisoned = true;
        let invocation = self.heap.begin_async_invocation();
        let main_id = self.main_thread;
        let args = args.to_vec();
        let outcome = catch_unwind_future(driver::run_async_function_protected(
            &mut self.heap,
            main_id,
            func,
            args,
            &governance,
            &self.app_data,
            SCRIPT_ERROR_TRACEBACK_MAX_BYTES,
        ))
        .await;
        let result = self.finish_async_invocation(invocation, outcome)?;
        let capture = self.take_traceback_capture();
        Ok(result.map_err(|failure| {
            ProtectedScriptError::from_failure(&mut self.heap, failure, capture)
        }))
    }

    /// Calls a registry-rooted callback on the async driver in protected mode.
    #[cfg(any())]
    pub(crate) async fn call_stashed_function_protected_async(
        &mut self,
        func: &scope::Stashed<ruau_vm_api::marker::Closure>,
        args: &[RawValue],
    ) -> Result<Result<Vec<RawValue>, ProtectedScriptError>, ruau_vm_api::Unwind> {
        let limits = self.limits.clone();
        self.call_stashed_function_protected_async_with_limits(func, args, limits)
            .await
    }

    /// Calls a registry-rooted callback on the async driver in protected mode
    /// with per-invocation resource ceilings.
    #[cfg(any())]
    pub(crate) async fn call_stashed_function_protected_async_with_limits(
        &mut self,
        func: &scope::Stashed<ruau_vm_api::marker::Closure>,
        args: &[RawValue],
        limits: Limits,
    ) -> Result<Result<Vec<RawValue>, ProtectedScriptError>, ruau_vm_api::Unwind> {
        if self.poisoned {
            return Err(Self::panic_poison_unwind());
        }
        let raw = self
            .heap
            .pinned_value(func.reference())
            .map_err(|message| self.runtime_unwind(message))?;
        match raw {
            RawValue::Function(_) => {
                self.call_function_protected_async_with_limits(raw, args, limits)
                    .await
            }
            _ => Err(self.runtime_unwind("stashed value is not a function")),
        }
    }

    #[cfg(any())]
    fn runtime_unwind(&mut self, message: impl Into<String>) -> ruau_vm_api::Unwind {
        let error = call::err(message);
        let kind = error.kind;
        ruau_vm_api::Unwind {
            error: call::materialize(&mut self.heap, error),
            kind,
        }
    }

    /// Marshals a protected-call outcome into the owned-exec return shape:
    /// returned values copied into an owned tree, a catchable script error
    /// mapped to [`ExecError::Script`], and a fatal unwind mapped to its
    /// [`ExecError`] variant. Shared by the sync and async owned-exec entries.
    fn finish_owned_exec(
        &self,
        outcome: Result<Result<Vec<RawValue>, ProtectedScriptError>, ruau_vm_api::Unwind>,
        limits: &Limits,
    ) -> Result<Vec<MarshaledValue>, ExecError> {
        match outcome {
            Ok(Ok(values)) => self
                .marshal_values_for_owned_entry(&values, limits)
                .map_err(|error| ExecError::from_marshal_error(&error)),
            Ok(Err(error)) => match self.try_marshal_protected_script_error(error, limits) {
                Ok(error) => Err(ExecError::Script(error)),
                Err(error) => Err(ExecError::from_marshal_error(&error)),
            },
            Err(error) => Err(self.exec_error_from_unwind(&error, limits)),
        }
    }

    /// Runs a loaded module synchronously in protected mode and owns its
    /// returned values.
    ///
    /// A successful script returns `Ok(values)` with immediate values, strings,
    /// buffers, and acyclic tables copied into an owned tree. A catchable
    /// script error returns [`ExecError::Script`] with a marshaled error value
    /// and traceback. Fatal control flow such as cancellation, deadline, or
    /// panic poison maps to dedicated [`ExecError`] variants.
    pub fn exec(
        &mut self,
        module: &LoadedModule,
        mut options: CallOptions,
    ) -> Result<Vec<MarshaledValue>, ExecError> {
        let limits = options.effective_limits(&self.limits);
        let restore = self.install_call_options(&mut options);
        let outcome = self.call_protected_with_effective_limits(module, &limits);
        let result = self.finish_owned_exec(outcome, &limits);
        self.restore_call_options(restore);
        result
    }

    /// Runs a loaded module synchronously in protected mode, owns its returned
    /// values, and lends a borrowed host context for the duration of the call.
    pub fn exec_with_context<T: Any>(
        &mut self,
        module: &LoadedModule,
        context: &mut T,
        mut options: CallOptions,
    ) -> Result<Vec<MarshaledValue>, ExecError> {
        let context = scope::ContextSlot::new(context);
        let _host_context = self.heap.enter_host_context(&context);
        let limits = options.effective_limits(&self.limits);
        let restore = self.install_call_options(&mut options);
        let outcome = self.call_protected_with_effective_limits(module, &limits);
        let result = self.finish_owned_exec(outcome, &limits);
        self.restore_call_options(restore);
        result
    }

    async fn call_protected_async_with_effective_limits(
        &mut self,
        module: &LoadedModule,
        limits: &Limits,
    ) -> Result<Result<Vec<RawValue>, ProtectedScriptError>, ruau_vm_api::Unwind> {
        if self.poisoned {
            return Err(Self::panic_poison_unwind());
        }
        self.begin_invocation(limits);
        let main = module.main;
        let governance = driver::Governance {
            deadline: match limits.deadline {
                Some(Deadline::Wall(instant)) => Some(instant),
                _ => None,
            },
            cancel: limits.cancel.clone(),
        };
        self.poisoned = true;
        let invocation = self.heap.begin_async_invocation();
        let main_id = self.main_thread;
        let outcome = catch_unwind_future(driver::run_async_protected(
            &mut self.heap,
            main_id,
            main,
            &governance,
            &self.app_data,
            SCRIPT_ERROR_TRACEBACK_MAX_BYTES,
        ))
        .await;
        let result = self.finish_async_invocation(invocation, outcome)?;
        let capture = self.take_traceback_capture();
        Ok(result.map_err(|failure| {
            ProtectedScriptError::from_failure(&mut self.heap, failure, capture)
        }))
    }

    /// Runs a loaded module on the async protected driver and owns its returned
    /// values.
    ///
    /// This is the async, owned-result entry point for ordinary embedders. It
    /// awaits async host calls and converts returned Lua values into
    /// [`MarshaledValue`] before the call boundary closes.
    ///
    /// # Errors
    /// Returns [`ExecError`] for load/run failures, catchable script errors,
    /// cancellation or deadline failures, value-marshal limits, table cycles,
    /// and allocation failures while copying the result vector.
    pub async fn exec_async(
        &mut self,
        module: &LoadedModule,
        mut options: CallOptions,
    ) -> Result<Vec<MarshaledValue>, ExecError> {
        let limits = options.effective_limits(&self.limits);
        let restore = self.install_call_options(&mut options);
        let outcome = self
            .call_protected_async_with_effective_limits(module, &limits)
            .await;
        let result = self.finish_owned_exec(outcome, &limits);
        self.restore_call_options(restore);
        result
    }

    /// Runs a loaded module on the async protected driver, owns its returned
    /// values, and lends a borrowed host context for the duration of the call.
    pub async fn exec_async_with_context<T: Any>(
        &mut self,
        module: &LoadedModule,
        context: &mut T,
        mut options: CallOptions,
    ) -> Result<Vec<MarshaledValue>, ExecError> {
        let context = scope::ContextSlot::new(context);
        let _host_context = self.heap.enter_host_context(&context);
        let limits = options.effective_limits(&self.limits);
        let restore = self.install_call_options(&mut options);
        let outcome = self
            .call_protected_async_with_effective_limits(module, &limits)
            .await;
        let result = self.finish_owned_exec(outcome, &limits);
        self.restore_call_options(restore);
        result
    }

    fn install_call_options(&mut self, options: &mut CallOptions) -> CallContextRestore {
        let app_data = options
            .app_data
            .take()
            .map(|app_data| self.app_data.replace(app_data));
        let print_sink = options
            .print_sink
            .take()
            .map(|sink| self.heap.replace_print_sink(Some(sink)));
        CallContextRestore {
            app_data,
            print_sink,
        }
    }

    fn restore_call_options(&mut self, restore: CallContextRestore) {
        if let Some(app_data) = restore.app_data {
            let _active = self.app_data.replace(app_data);
        }
        if let Some(print_sink) = restore.print_sink {
            let _active = self.heap.replace_print_sink(print_sink);
        }
    }

    fn exec_error_from_unwind(&self, error: &ruau_vm_api::Unwind, limits: &Limits) -> ExecError {
        match error.kind {
            RuntimeErrorKind::Cancelled => ExecError::Cancelled,
            RuntimeErrorKind::Deadline => ExecError::Deadline,
            RuntimeErrorKind::PanicPoison => ExecError::PanicPoison,
            _ => match self.try_marshal_unwind_error(error, limits) {
                Ok(error) => ExecError::Script(error),
                Err(error) => ExecError::from_marshal_error(&error),
            },
        }
    }

    fn marshal_values_for_owned_entry(
        &self,
        values: &[RawValue],
        limits: &Limits,
    ) -> Result<Vec<MarshaledValue>, ValueMarshalError> {
        let marshal_limits = ValueMarshalLimits::from(limits.effective());
        let mut visitor = ValueVisitor::new(&self.heap, marshal_limits);
        visitor.visit_values(values)
    }

    fn try_marshal_protected_script_error(
        &self,
        error: ProtectedScriptError,
        limits: &Limits,
    ) -> Result<MarshaledScriptError, ValueMarshalError> {
        let marshal_limits = ValueMarshalLimits::from(limits.effective());
        let mut visitor = ValueVisitor::new(&self.heap, marshal_limits);
        let value = visitor.visit_value(error.value)?;
        Ok(MarshaledScriptError::new(
            value,
            error.kind(),
            error.traceback().map(str::to_owned),
            error.frames,
            error.frames_truncated,
            error.payload,
        ))
    }

    fn try_marshal_unwind_error(
        &self,
        error: &ruau_vm_api::Unwind,
        limits: &Limits,
    ) -> Result<MarshaledScriptError, ValueMarshalError> {
        let mut values = self.marshal_values_for_owned_entry(&[error.error], limits)?;
        Ok(MarshaledScriptError::new(
            values.pop().unwrap_or(MarshaledValue::Nil),
            error.kind,
            None,
            Vec::new(),
            false,
            None,
        ))
    }

    /// Takes the structured traceback most recently stashed on the main thread
    /// by a protected unwind, for re-pairing with the failure it was captured
    /// for (see [`ProtectedScriptError::from_failure`]). `None` when the main
    /// thread is unavailable or no capture is stashed.
    fn take_traceback_capture(&mut self) -> Option<debug::Traceback> {
        self.heap
            .thread_mut(self.main_thread)
            .and_then(|thread| thread.captured_traceback.take())
    }
    /// Calls a Lua function value with `args`, running it to completion and
    /// returning its results — the host's entry into a loaded closure.
    ///
    /// This trusted embedder entry point rejects stale, dangling, and cross-VM
    /// raw handles.
    ///
    /// # Errors
    /// Returns the [`ruau_vm_api::Unwind`] of an uncaught runtime error, or a contained
    /// panic (after which the VM is poisoned).
    #[cfg(any(test, feature = "conformance"))]
    pub fn call_function(
        &mut self,
        func: RawValue,
        args: &[RawValue],
        mut options: CallOptions,
    ) -> Result<Vec<RawValue>, ruau_vm_api::Unwind> {
        let limits = options.effective_limits(&self.limits);
        let restore = self.install_call_options(&mut options);
        let result = self.call_function_with_effective_limits(func, args, &limits);
        self.restore_call_options(restore);
        result
    }

    #[cfg(any(test, feature = "conformance"))]
    fn call_function_with_effective_limits(
        &mut self,
        func: RawValue,
        args: &[RawValue],
        limits: &Limits,
    ) -> Result<Vec<RawValue>, ruau_vm_api::Unwind> {
        if self.poisoned {
            return Err(Self::panic_poison_unwind());
        }
        self.begin_invocation(limits);
        let result = self.contained(|heap, thread| call::run_function(heap, thread, func, args));
        if !self.poisoned {
            self.finish_invocation();
        }
        result
    }

    /// Whether a host-boundary call has poisoned this VM with a contained panic.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Human-readable reason the VM was poisoned before or during execution, if
    /// the entry point that poisoned it could preserve one.
    #[must_use]
    pub fn poison_reason(&self) -> Option<&str> {
        self.poison_reason.as_deref()
    }

    /// Runs `body` behind the host-call boundary's panic guard (§8.5): a panic in
    /// the VM is caught so it cannot crash a multi-tenant worker, the VM is marked
    /// poisoned (its state may be inconsistent), and the host gets an error. The
    /// caught error value is `nil` because the heap is unsafe to touch after a
    /// mid-mutation panic; the panic itself is reported through the default hook.
    fn contained<R>(
        &mut self,
        body: impl FnOnce(&mut VmHeap, &mut state::Thread) -> Result<R, ruau_vm_api::Unwind>,
    ) -> Result<R, ruau_vm_api::Unwind> {
        if self.poisoned {
            return Err(Self::panic_poison_unwind());
        }
        // Take the live main thread out of the arena so its register stack is a
        // disjoint borrow from the heap objects again (the same take-out/put-back a
        // coroutine resume uses); put it back after the guard — the thread is owned
        // here across `catch_unwind`, so the slot is restored even on a caught panic
        // (and the VM is poisoned either way, so a lost slot is unobservable).
        let main_id = self.main_thread;
        let Some(mut thread) = self.heap.take_thread(main_id) else {
            self.poisoned = true;
            return Err(Self::panic_poison_unwind());
        };
        let _host_app_data = self.heap.enter_host_app_data(&self.app_data);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            body(&mut self.heap, &mut thread)
        }));
        if !self.heap.put_thread(main_id, thread) {
            // The main thread's slot is a GC root, reserved for its whole run, so
            // this should be unreachable; if it ever isn't, poison rather than
            // continue with a hollow main thread. (Not `.expect`: this is past
            // `catch_unwind`, so a panic here would abort the process.)
            self.poisoned = true;
        }
        match outcome {
            Ok(result) => result,
            Err(_) => {
                self.poisoned = true;
                Err(Self::panic_poison_unwind())
            }
        }
    }

    fn panic_poison_unwind() -> ruau_vm_api::Unwind {
        ruau_vm_api::Unwind {
            error: RawValue::Nil,
            kind: RuntimeErrorKind::PanicPoison,
        }
    }
    /// Settles one finished async invocation the way every async entry point
    /// does: an orderly outcome (success or catchable failure) ends the
    /// invocation epoch, clears the poison flag, and restores the default
    /// limits; a fatal control error additionally runs its cleanup; a panic
    /// (or a fatal error that already carries the poison kind) leaves the VM
    /// poisoned.
    fn finish_async_invocation<T, P>(
        &mut self,
        invocation: u64,
        outcome: Result<Result<T, ruau_vm_api::Unwind>, P>,
    ) -> Result<T, ruau_vm_api::Unwind> {
        match outcome {
            Ok(Ok(value)) => {
                self.heap.end_async_invocation(invocation);
                self.poisoned = false;
                self.finish_invocation();
                Ok(value)
            }
            Ok(Err(error)) if error.kind != RuntimeErrorKind::PanicPoison => {
                self.cleanup_fatal_async_control(error.kind, invocation);
                self.heap.end_async_invocation(invocation);
                self.poisoned = false;
                self.finish_invocation();
                Err(error)
            }
            Ok(Err(_)) | Err(_) => {
                self.heap.end_async_invocation(invocation);
                self.poisoned = true;
                Err(Self::panic_poison_unwind())
            }
        }
    }

    fn cleanup_fatal_async_control(&mut self, kind: RuntimeErrorKind, invocation: u64) {
        if matches!(
            kind,
            RuntimeErrorKind::Cancelled | RuntimeErrorKind::Deadline
        ) {
            self.heap.abort_invocation_coroutines(invocation);
        }
    }

    fn begin_invocation(&mut self, limits: &Limits) {
        self.execution_count = self.execution_count.saturating_add(1);
        self.heap.reset_gas_spent();
        self.heap.begin_gas_profile(limits.gas_profile);
        self.apply_invocation_limits(limits);
    }

    fn finish_invocation(&mut self) {
        self.heap.finish_gas_profile();
        self.apply_default_limits();
    }

    fn apply_default_limits(&mut self) {
        let limits = self.limits.clone();
        self.apply_invocation_limits(&limits);
    }

    fn apply_invocation_limits(&mut self, limits: &Limits) {
        let logical_deadline = match limits.deadline {
            Some(Deadline::Logical(tick)) => Some(tick),
            _ => None,
        };
        // The logical deadline reads `gas_spent` as its clock, and the counter
        // only ticks while gas metering is on — so a logical deadline with no
        // gas ceiling meters against an unlimited budget.
        let gas = match (limits.gas, logical_deadline) {
            (None, Some(_)) => Some(u64::MAX),
            (gas, _) => gas,
        };
        self.heap.set_gas(gas);
        self.heap.set_logical_deadline(logical_deadline);
        self.heap.set_quantum(limits.quantum);
        self.heap.set_memory_cap(limits.max_memory_bytes);
        self.heap.set_cancel(limits.cancel.clone());
        let effective = limits.effective();
        self.heap.set_limits(effective);
    }

    /// The owning heap, mutably — for trusted host setup such as building tables
    /// and attaching metatables before a call. Raw handles returned through this
    /// API are engine/embedder handles; do not expose them as tenant capabilities.
    #[cfg(any(test, feature = "conformance"))]
    pub fn heap_mut(&mut self) -> &mut VmHeap {
        &mut self.heap
    }
}

/// The trusted Lua prelude installed at build. `coroutine.wrap` is defined here
/// rather than as an engine builtin because it must capture its coroutine in an
/// upvalue, which a `Builtin` closure (no captured state) cannot do; a Lua
/// closure can. The entry trampoline is also intentional: wrapped builtin
/// functions must be reached through ordinary bytecode `CALL` handling, where
/// control builtins such as `pcall` can use their yieldable shared-stack paths.
const PRELUDE: &str = r#"
-- Capture the trusted stdlib functions as upvalues so a tenant that later
-- overwrites `table.pack`/`coroutine.resume`/etc. cannot change wrap's behavior
-- (upstream's wrap is a C closure, immune to such global mutation).
local create = coroutine.create
local resume = coroutine.resume
local pack = table.pack
local unpack = table.unpack
local raise = error
local kind = type

function coroutine.wrap(f)
    if kind(f) ~= "function" then
        raise("bad argument #1 to 'coroutine.wrap' (function expected)", 0)
    end
    local co = create(function(...)
        return f(...)
    end)
    return function(...)
        local results = pack(resume(co, ...))
        if not results[1] then
            -- Re-raise from the wrapper's caller. String errors gain the wrap
            -- call-site prefix; non-string error objects still surface unchanged,
            -- matching `error` itself.
            raise(results[2], 2)
        end
        return unpack(results, 2, results.n)
    end
end
"#;

/// The compiled prelude, built once per process and reused by every VM.
fn prelude_chunk() -> &'static BytecodeChunk {
    static CHUNK: std::sync::OnceLock<BytecodeChunk> = std::sync::OnceLock::new();
    CHUNK.get_or_init(|| {
        compile_source(PRELUDE, &CompileOptions::default(), None).expect("the prelude compiles")
    })
}

/// Interns `name` and sets `table[name] = value`. Used for library members and
/// the handful of library constants (`math.pi`, `utf8.charpattern`, …).
fn set_member(
    heap: &mut VmHeap,
    table: ruau_vm_api::RawGc<ruau_vm_api::marker::Table>,
    name: &[u8],
    value: RawValue,
) -> Option<()> {
    let key = RawValue::String(heap.intern_str(name)?);
    heap.table_mut(table)?.set(key, value);
    Some(())
}

/// Allocates a library table, fills it with its engine builtins, and installs it
/// in `globals` under `name`. Returns the new table so the caller can add any
/// library-specific constants. Allocation failure returns `None`.
fn install_library(
    heap: &mut VmHeap,
    globals: ruau_vm_api::RawGc<ruau_vm_api::marker::Table>,
    name: &[u8],
    members: &[builtins::Builtin],
) -> Option<ruau_vm_api::RawGc<ruau_vm_api::marker::Table>> {
    let library = heap.alloc_table(VmLuaTable::new())?;
    for &builtin in members {
        let closure = heap.alloc_builtin(builtin)?;
        set_member(
            heap,
            library,
            builtin.global_name(),
            RawValue::Function(closure),
        )?;
    }
    set_member(heap, globals, name, RawValue::Table(library))?;
    Some(library)
}

/// Builds the global table: the base globals (`assert`/`type`/`tostring`/
/// `tonumber`/`error`/`print`/`setmetatable`/`getmetatable`/`pcall`/`raw*`/`next`/
/// `pairs`/`ipairs`), always present, plus each optional library table the
/// runtime capabilities select. A loaded chunk resolves these through
/// `GETGLOBAL`/`GETIMPORT`. Allocation failure returns `None`.
fn install_base_globals(
    heap: &mut VmHeap,
    capabilities: &RuntimeCapabilities,
) -> Option<ruau_vm_api::RawGc<ruau_vm_api::marker::Table>> {
    use runtime_capabilities::Library;

    let table = heap.alloc_table(VmLuaTable::new())?;
    install_core_globals(heap, table, capabilities)?;

    if capabilities.includes(Library::Coroutine) {
        install_library(
            heap,
            table,
            b"coroutine",
            &builtins::Builtin::coroutine_members(),
        )?;
    }
    if capabilities.includes(Library::String) {
        install_string_library(heap, table)?;
    }
    if capabilities.includes(Library::Math) {
        install_math_library(heap, table)?;
    }
    if capabilities.includes(Library::Integer) {
        install_integer_library(heap, table)?;
    }
    if capabilities.includes(Library::Table) {
        install_library(heap, table, b"table", &builtins::Builtin::table_members())?;
    }
    if capabilities.includes(Library::Bit32) {
        install_library(heap, table, b"bit32", &builtins::Builtin::bit32_members())?;
    }
    if capabilities.includes(Library::Utf8) {
        install_utf8_library(heap, table)?;
    }
    if capabilities.includes(Library::Os) {
        install_library(heap, table, b"os", &builtins::Builtin::os_members())?;
    }
    if capabilities.includes(Library::Buffer) {
        install_library(heap, table, b"buffer", &builtins::Builtin::buffer_members())?;
    }
    if capabilities.includes(Library::Vector) {
        install_vector_library(heap, table)?;
    }
    if capabilities.includes(Library::Debug) {
        install_library(heap, table, b"debug", &builtins::Builtin::debug_members())?;
    }
    Some(table)
}

/// Installs the base globals (the always-present language core) plus the
/// `unpack` alias for `table.unpack`, which upstream's base library exports
/// directly and so is present regardless of whether the `table` library is.
fn install_core_globals(
    heap: &mut VmHeap,
    table: ruau_vm_api::RawGc<ruau_vm_api::marker::Table>,
    capabilities: &RuntimeCapabilities,
) -> Option<()> {
    for builtin in builtins::Builtin::all() {
        if builtin == builtins::Builtin::Loadstring && !capabilities.runtime_compilation_enabled() {
            continue;
        }
        // `require` is installed only when an embedder supplied a source or a
        // native module export; otherwise it stays absent.
        if builtin == builtins::Builtin::Require && !heap.require_available() {
            continue;
        }
        let closure = heap.alloc_builtin(builtin)?;
        set_member(
            heap,
            table,
            builtin.global_name(),
            RawValue::Function(closure),
        )?;
    }
    let unpack = heap.alloc_builtin(builtins::Builtin::TableUnpack)?;
    set_member(heap, table, b"unpack", RawValue::Function(unpack))?;
    Some(())
}

/// Installs the `string` library together with the shared string metatable
/// whose `__index` is the library itself, so a string value resolves methods
/// through it (`("s"):upper()`).
fn install_string_library(
    heap: &mut VmHeap,
    table: ruau_vm_api::RawGc<ruau_vm_api::marker::Table>,
) -> Option<()> {
    let string = install_library(heap, table, b"string", &builtins::Builtin::string_members())?;
    let string_mt = heap.alloc_table(VmLuaTable::new())?;
    set_member(heap, string_mt, b"__index", RawValue::Table(string))?;
    // The freshly allocated table is live, so this validates trivially; `.ok()?`
    // folds the (unreachable here) rejection into the build's allocation-failure path.
    heap.set_string_metatable(string_mt).ok()?;
    Some(())
}

/// Installs the `math` library and its numeric constants.
fn install_math_library(
    heap: &mut VmHeap,
    table: ruau_vm_api::RawGc<ruau_vm_api::marker::Table>,
) -> Option<()> {
    let math = install_library(heap, table, b"math", &builtins::Builtin::math_members())?;
    set_member(heap, math, b"huge", RawValue::Number(f64::INFINITY))?;
    for (name, value) in [
        (b"pi".as_slice(), std::f64::consts::PI),
        (b"tau", std::f64::consts::TAU),
        (b"e", std::f64::consts::E),
        (b"phi", 1.618_033_988_749_895_f64),
        (b"sqrt2", std::f64::consts::SQRT_2),
        (b"nan", f64::NAN),
    ] {
        set_member(heap, math, name, RawValue::Number(value))?;
    }
    Some(())
}

/// Installs the `integer` library and its signed-bound constants.
fn install_integer_library(
    heap: &mut VmHeap,
    table: ruau_vm_api::RawGc<ruau_vm_api::marker::Table>,
) -> Option<()> {
    let integer = install_library(
        heap,
        table,
        b"integer",
        &builtins::Builtin::integer_members(),
    )?;
    set_member(heap, integer, b"minsigned", RawValue::Integer(i64::MIN))?;
    set_member(heap, integer, b"maxsigned", RawValue::Integer(i64::MAX))?;
    Some(())
}

/// Installs the `utf8` library and its `charpattern` constant.
fn install_utf8_library(
    heap: &mut VmHeap,
    table: ruau_vm_api::RawGc<ruau_vm_api::marker::Table>,
) -> Option<()> {
    let utf8 = install_library(heap, table, b"utf8", &builtins::Builtin::utf8_members())?;
    let charpattern = RawValue::String(heap.intern_str(b"[\0-\x7F\xC2-\xF4][\x80-\xBF]*")?);
    set_member(heap, utf8, b"charpattern", charpattern)?;
    Some(())
}

/// Installs the `vector` library and its `zero`/`one` constants.
fn install_vector_library(
    heap: &mut VmHeap,
    table: ruau_vm_api::RawGc<ruau_vm_api::marker::Table>,
) -> Option<()> {
    let vector = install_library(heap, table, b"vector", &builtins::Builtin::vector_members())?;
    set_member(heap, vector, b"zero", RawValue::Vector([0.0, 0.0, 0.0]))?;
    set_member(heap, vector, b"one", RawValue::Vector([1.0, 1.0, 1.0]))?;
    Some(())
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn vm_is_send_for_the_m_n_lane_pool() {
        // The M:N lane pool moves a VM-at-rest between lanes (a `Send` `VmHandle`),
        // so `Vm` — and therefore its `Heap` (print sink, release channel, app
        // data, module source, …) — must be `Send`. This static assertion locks
        // that precondition in: a future `!Send` field fails to compile here, not
        // deep in the pool. (Not `Sync`: a VM is owned by one lane at a time, never
        // shared.)
        fn assert_send<T: Send>() {}
        assert_send::<Vm>();
    }

    #[test]
    fn builder_fails_closed_on_each_unset_field() {
        // No implicit default for the four required fields, in order. `.err()`
        // (not `unwrap_err`) avoids requiring `Vm: Debug` for the `Ok` arm.
        assert_eq!(
            Vm::builder().build().err(),
            Some(VmBuildError::MissingAmbient)
        );
        assert_eq!(
            Vm::builder()
                .ambient(Ambient::deterministic(0))
                .build()
                .err(),
            Some(VmBuildError::MissingLimits)
        );
        assert_eq!(
            Vm::builder()
                .ambient(Ambient::deterministic(0))
                .limits(Limits::unlimited())
                .build()
                .err(),
            Some(VmBuildError::MissingRuntimeCapabilities)
        );
        assert_eq!(
            Vm::builder()
                .ambient(Ambient::deterministic(0))
                .limits(Limits::unlimited())
                .runtime_capabilities(RuntimeCapabilities::default().enable_runtime_compilation())
                .build()
                .err(),
            Some(VmBuildError::MissingSandboxPolicy)
        );
        // All four set → builds.
        assert!(
            Vm::builder()
                .ambient(Ambient::deterministic(0))
                .limits(Limits::unlimited())
                .runtime_capabilities(RuntimeCapabilities::default().enable_runtime_compilation())
                .trusted_host()
                .build()
                .is_ok()
        );
        assert_eq!(
            VmBuildError::MissingRuntimeCapabilities.to_string(),
            "VM builder is missing the required `runtime_capabilities` configuration"
        );
        assert_eq!(
            VmBuildError::MissingSandboxPolicy.to_string(),
            "VM builder is missing a sandbox policy: call `sandboxed()` or `trusted_host()`"
        );
    }

    #[test]
    fn step_refuses_a_poisoned_vm() {
        let mut vm = test_vm();
        vm.poisoned = true;
        let err = vm
            .step(|_s| Ok(()))
            .expect_err("a poisoned VM refuses a scope step");
        assert_eq!(err.kind(), RuntimeErrorKind::PanicPoison);
    }

    #[test]
    fn builder_installs_globals_and_a_unique_heap_nonce() {
        let vm = Vm::builder()
            .ambient(Ambient::deterministic(7))
            .build_for_test();
        assert!(vm.main_thread().unwrap().call_stack.is_empty());
        // The build installs the base globals: the main thread points at a global
        // table, and that table is resident.
        assert!(vm.main_thread().unwrap().globals.is_some());
        assert!(!vm.heap().objects().tables.is_empty());
        // The heap nonce is decoupled from the hash seed.
        assert_ne!(vm.heap().id, HeapId(7));
    }

    #[test]
    fn collect_skips_a_poisoned_vm() {
        let mut vm = test_vm();
        vm.poisoned = true;
        // With no resident main thread to root, an unguarded collection would sweep
        // the live heap; the guard must report the skip reason instead.
        assert_eq!(
            vm.collect(),
            CollectionOutcome::SkippedPoisoned,
            "a poisoned VM must not collect"
        );
    }

    #[test]
    fn collect_reports_completed_empty_cycle() {
        let mut vm = test_vm();
        vm.collect();
        assert_eq!(
            vm.collect(),
            CollectionOutcome::Completed {
                kind: CollectionKind::Major,
                reclaimed: 0
            },
            "a completed no-op cycle is distinct from a skipped collection"
        );
    }

    #[test]
    fn routine_collection_observation_reports_minor_or_major_choice() {
        let mut vm = test_vm();
        let observed = vm.collect_routine();
        assert!(
            matches!(
                observed,
                CollectionOutcome::Completed {
                    kind: CollectionKind::Minor,
                    ..
                } | CollectionOutcome::Completed {
                    kind: CollectionKind::Major,
                    ..
                }
            ),
            "routine collection reports the generational path it used: {observed:?}"
        );
    }

    #[test]
    fn collect_step_accumulates_work_and_runs_one_routine_cycle() {
        let mut vm = test_vm();
        let cycles = vm.heap().gc_cycles();
        assert_eq!(vm.collect_step(1), CollectionStepOutcome::Pending);
        assert_eq!(
            vm.heap().gc_cycles(),
            cycles,
            "a pending step must not collect"
        );

        let outcome = vm.collect_step(11);
        assert!(
            matches!(
                outcome,
                CollectionStepOutcome::Collection(CollectionOutcome::Completed {
                    kind: CollectionKind::Minor,
                    ..
                }) | CollectionStepOutcome::Collection(CollectionOutcome::Completed {
                    kind: CollectionKind::Major,
                    ..
                })
            ),
            "a completed host step runs one routine collection: {outcome:?}"
        );
        assert_eq!(vm.heap().gc_cycles(), cycles + 1);
    }

    #[test]
    fn collect_step_services_pending_script_gc_requests() {
        let mut vm = test_vm();
        vm.heap_mut().request_gc();
        assert!(matches!(
            vm.collect_step(1),
            CollectionStepOutcome::Collection(CollectionOutcome::Completed {
                kind: CollectionKind::Major,
                ..
            })
        ));
        assert!(
            !vm.heap_mut().take_gc_request(),
            "host-paced GC consumes the pending script request"
        );
    }

    #[test]
    fn collect_step_reports_collection_skip_reasons() {
        let mut vm = test_vm();
        vm.poisoned = true;
        assert_eq!(
            vm.collect_step(12),
            CollectionStepOutcome::Collection(CollectionOutcome::SkippedPoisoned)
        );

        let mut vm = test_vm();
        let main = vm.main_thread;
        let thread = vm.heap.take_thread(main).expect("take main thread");
        assert_eq!(
            vm.collect_step(12),
            CollectionStepOutcome::Collection(CollectionOutcome::SkippedMainThreadUnavailable)
        );
        assert!(vm.heap.put_thread(main, thread));
    }

    fn snapshot_test_builder(seed: u64) -> VmBuilder {
        Vm::builder()
            .ambient(Ambient::deterministic(seed))
            .limits(Limits {
                gas: Some(10_000),
                ..Limits::unlimited()
            })
            .runtime_capabilities(RuntimeCapabilities::default())
            .trusted_host()
    }

    fn install_snapshot_script(vm: &mut Vm) {
        let chunk = compile_source(
            r#"
local STATE = { n = 0, bag = { alpha = 11, beta = 17 } }

function advance(delta)
    STATE.n += delta
    STATE.bag[delta] = STATE.n
    local total = 0
    for _, value in pairs(STATE.bag) do
        total += value
    end
    return STATE.n, total, tostring(STATE.bag), math.random()
end
"#,
            &CompileOptions::default(),
            None,
        )
        .expect("compile snapshot script");
        let module = vm.load_named(&chunk, b"=snapshot").expect("load script");
        vm.call(&module, Default::default())
            .expect("script installs globals");
    }

    fn call_advance(vm: &mut Vm, delta: i64) -> (i64, i64, String, f64, u64) {
        let source = format!("return advance({delta})");
        let chunk = compile_source(&source, &CompileOptions::default(), None)
            .expect("compile advance thunk");
        let module = vm
            .load_named(&chunk, b"=advance-call")
            .expect("load advance thunk");
        let results = vm
            .call(&module, Default::default())
            .expect("advance call succeeds");
        let [n, total, identity, random] = results.as_slice() else {
            panic!("advance returned {results:?}");
        };
        let n = raw_to_i64(*n);
        let total = raw_to_i64(*total);
        let identity = match *identity {
            RawValue::String(handle) => String::from_utf8_lossy(
                vm.heap()
                    .string(handle)
                    .expect("identity string resolves")
                    .bytes(),
            )
            .into_owned(),
            other => panic!("expected identity string, got {other:?}"),
        };
        let random = match *random {
            RawValue::Number(value) => value,
            other => panic!("expected random number, got {other:?}"),
        };
        (n, total, identity, random, vm.heap().gas_spent())
    }

    fn raw_to_i64(value: RawValue) -> i64 {
        match value {
            RawValue::Integer(value) => value,
            RawValue::Number(value) => value as i64,
            other => panic!("expected numeric integer result, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_restore_replays_quiescent_sandbox_state() {
        let mut vm = snapshot_test_builder(91).build().expect("build vm");
        install_snapshot_script(&mut vm);
        vm.sandbox_for_untrusted().expect("sandbox");
        assert_eq!(call_advance(&mut vm, 3).0, 3);

        let snapshot = vm.snapshot().expect("snapshot");
        let mut restored = snapshot_test_builder(91)
            .restore_snapshot(snapshot.as_bytes())
            .expect("restore snapshot");

        let original_next = call_advance(&mut vm, 5);
        let restored_next = call_advance(&mut restored, 5);
        assert_eq!(
            restored_next, original_next,
            "run-through and snapshot+restore must agree on data, identity text, RNG, and gas"
        );

        let original_after = call_advance(&mut vm, 7);
        let restored_after = call_advance(&mut restored, 7);
        assert_eq!(restored_after, original_after);
    }

    #[test]
    fn snapshot_restore_rejects_corrupted_bytes() {
        let mut vm = snapshot_test_builder(17).build().expect("build vm");
        install_snapshot_script(&mut vm);
        vm.sandbox_for_untrusted().expect("sandbox");
        let mut bytes = vm.snapshot().expect("snapshot").into_bytes();
        bytes.truncate(bytes.len() / 2);

        assert!(
            matches!(
                snapshot_test_builder(17).restore_snapshot(&bytes),
                Err(SnapshotError::Decode(_))
            ),
            "truncated snapshots must fail closed"
        );
    }

    #[test]
    fn snapshot_restore_rejects_template_mismatch() {
        let mut vm = snapshot_test_builder(17).build().expect("build vm");
        install_snapshot_script(&mut vm);
        vm.sandbox_for_untrusted().expect("sandbox");
        let snapshot = vm.snapshot().expect("snapshot");

        assert!(
            matches!(
                snapshot_test_builder(18).restore_snapshot(snapshot.as_bytes()),
                Err(SnapshotError::TemplateMismatch("ambient"))
            ),
            "a different deterministic seed is a different restore template"
        );
    }

    #[test]
    fn collect_reports_unavailable_main_thread() {
        let mut vm = test_vm();
        let main = vm.main_thread;
        let thread = vm.heap.take_thread(main).expect("take main thread");
        assert_eq!(
            vm.collect(),
            CollectionOutcome::SkippedMainThreadUnavailable,
            "manual collection must not run with the main thread taken out"
        );
        assert!(vm.heap.put_thread(main, thread));
    }

    #[test]
    fn collect_reclaims_unrooted_but_keeps_the_live_global_graph() {
        use crate::table::LuaTable;
        let mut vm = test_vm();
        let globals = vm
            .main_thread()
            .unwrap()
            .globals
            .expect("globals installed");
        // An unreachable table: nothing in the live graph references it.
        let garbage = vm.heap_mut().alloc_table(LuaTable::new()).expect("alloc");
        let freed = vm.collect().reclaimed();
        assert!(freed >= 1, "the unrooted table is reclaimed");
        assert!(
            vm.heap().table(garbage).is_none(),
            "the unrooted handle is now stale"
        );
        assert!(
            vm.heap().table(globals).is_some(),
            "the global table, rooted via the main thread, survives"
        );
        // The whole stdlib graph reachable from globals survives a collection.
        assert!(matches!(
            global_value(&mut vm, b"print"),
            RawValue::Function(_)
        ));
        // And the post-collection heap is consistent: no live object holds a handle to
        // a slot the collection freed.
        vm.validate()
            .expect("the real stdlib heap is consistent after a collection");
    }

    #[test]
    fn a_loaded_module_survives_collection_until_unloaded() {
        let chunk = compile_source("return 1", &CompileOptions::default(), None).expect("compile");
        let mut vm = test_vm();
        let module = vm.load(&chunk).expect("load");
        let main = module.main;
        // The module's main closure is rooted by its registry pin, so it survives a
        // collection even though no Lua value references it — a host can load, collect,
        // then call without a use-after-free.
        assert!(vm.collect().completed());
        assert!(
            vm.heap().closure(main).is_some(),
            "a held module survives collection (pinned in the registry)"
        );
        // Unloading releases the pin; the closure (and its proto graph) is now garbage.
        vm.unload(module);
        let freed = vm.collect().reclaimed();
        assert!(freed >= 1, "unloading made the module collectable");
        assert!(
            vm.heap().closure(main).is_none(),
            "the unloaded module is reclaimed"
        );
    }

    #[test]
    fn loading_charges_proto_footprint_and_collection_releases_it() {
        let chunk = compile_source(
            "local function f(a, b) return a + b end return f(1, 2)",
            &CompileOptions::default(),
            None,
        )
        .expect("compile");
        let mut vm = test_vm();
        let before = vm.heap().total_bytes();
        let module = vm.load(&chunk).expect("load");
        assert!(
            vm.heap().total_bytes() > before,
            "loading a module charges its prototype footprint against the cap"
        );
        let after_load = vm.heap().total_bytes();
        // Unload and collect: the module's prototypes (and their buffers) are reclaimed,
        // dropping the charge back out — loaded bytecode no longer evades the cap.
        vm.unload(module);
        assert!(vm.collect().completed());
        assert!(
            vm.heap().total_bytes() < after_load,
            "collecting the unloaded module releases its proto footprint"
        );
    }

    #[test]
    fn loading_over_the_memory_cap_fails_eagerly() {
        let chunk = compile_source(
            "local function f(a, b, c) local x = a + b + c return x * x - a end return f(1, 2, 3)",
            &CompileOptions::default(),
            None,
        )
        .expect("compile");
        // Measure the module's footprint with no cap.
        let mut probe = test_vm();
        let base = probe.heap().total_bytes();
        probe.load(&chunk).expect("load without a cap");
        let module_bytes = probe.heap().total_bytes() - base;
        assert!(module_bytes > 0, "the module charges a footprint");

        // Load the same chunk under a cap below the module's footprint: the loader must reject
        // it eagerly with `OutOfMemory` rather than charging its prototypes past the cap and
        // leaving a module that trips the limit only at the next runtime safepoint.
        let mut vm = test_vm();
        let cap = vm.heap().total_bytes() + module_bytes / 2;
        vm.heap_mut().set_memory_cap(Some(cap));
        assert!(
            matches!(vm.load(&chunk), Err(LoadError::OutOfMemory)),
            "loading a module that would exceed the cap fails eagerly"
        );
    }

    #[test]
    fn unloading_a_module_on_the_wrong_vm_is_a_no_op() {
        let chunk = compile_source("return 1", &CompileOptions::default(), None).expect("compile");
        let mut vm_a = test_vm();
        let mut vm_b = test_vm();
        // Each VM pins its own module (both at slot 0 of their own registries).
        let module_a = vm_a.load(&chunk).expect("load a");
        let module_b = vm_b.load(&chunk).expect("load b");
        let b_main = module_b.main;
        // Unloading A's module on B is a cross-VM misuse: A's ref is heap-branded, so B
        // ignores it. Without the brand, B would free its own same-numbered slot and
        // sweep B's still-live module — a use-after-free.
        vm_b.unload(module_a);
        assert!(vm_b.collect().completed());
        assert!(
            vm_b.heap().closure(b_main).is_some(),
            "a wrong-VM unload must not release this VM's same-numbered slot"
        );
    }

    #[test]
    fn conformance_integer_scripts_enable_integer_type_compilation() {
        let options = conformance_compile_options_for_script("integers.luau");
        assert!(options.syntax_flags.luau_integer_type);
        assert!(options.fast_flag("LuauIntegerType"));
        let chunk =
            ruau_bytecode::compile_source_strict_with_upstream_options("return 1i", &options, None)
                .expect("compile integer literal");
        assert!(matches!(
            chunk,
            BytecodeChunk::Valid {
                bytecode_version: 8,
                ..
            }
        ));

        let ordinary = conformance_compile_options_for_script("classes.luau");
        assert!(!ordinary.syntax_flags.luau_integer_type);
        assert!(!ordinary.fast_flag("LuauIntegerType"));
        assert_eq!(ordinary.coverage_level, 0);
        let chunk =
            ruau_bytecode::compile_source_strict_with_upstream_options("return 1", &ordinary, None)
                .expect("compile ordinary number literal");
        assert!(matches!(
            chunk,
            BytecodeChunk::Valid {
                bytecode_version: 7,
                ..
            }
        ));

        let coverage = conformance_compile_options_for_script("coverage.luau");
        assert_eq!(coverage.coverage_level, 1);
    }

    #[test]
    fn conformance_config_carries_limits_compile_options_and_features() {
        let config = conformance_config_for_script("integers.luau");
        assert_eq!(config.limits.gas, Some(CONFORMANCE_GAS));
        assert!(config.compile_options.syntax_flags.luau_integer_type);
        assert!(config.compile_options.fast_flag("LuauIntegerType"));
        assert_eq!(
            config.features,
            ExecutionFeatures {
                harness_mode: true,
                ..ExecutionFeatures::all_off()
            }
        );
    }

    #[test]
    fn conformance_harness_features_are_explicit_per_script() {
        for name in [
            "calls.luau",
            "closure.luau",
            "constructs.luau",
            "errors.luau",
            "gc.luau",
            "literals.luau",
            "locals.luau",
            "math.luau",
            "pm.luau",
            "utf8.luau",
            "vararg.luau",
        ] {
            assert!(
                conformance_config_for_script(name).runtime_compilation,
                "{name} should opt into runtime-compilation compatibility"
            );
        }

        for name in [
            "buffers.luau",
            "calls.luau",
            "coroutine.luau",
            "coverage.luau",
            "cyield.luau",
            "errors.luau",
            "gc.luau",
            "integers.luau",
            "iter.luau",
            "pcall.luau",
            "types.luau",
            "vector.luau",
            "vector_library.luau",
        ] {
            assert!(
                conformance_config_for_script(name).features.harness_mode,
                "{name} should opt into conformance harness helpers"
            );
        }

        for name in [
            "basic.luau",
            "buffers.luau",
            "closure.luau",
            "events.luau",
            "iter_fenv.luau",
            "locals.luau",
            "safeenv.luau",
            "tables.luau",
        ] {
            assert!(
                conformance_config_for_script(name).features.fenv,
                "{name} should opt into fenv compatibility"
            );
        }

        let ordinary = conformance_config_for_script("debug.luau");
        assert_eq!(ordinary.features, ExecutionFeatures::all_off());

        let owned_harness = include_bytes!("../conformance-ruau/gc_block_allocations.luau");
        let config = conformance_config_for_script_source(
            "gc_block_allocations.luau",
            owned_harness,
            ConformanceScriptOrigin::RuauOwned,
        )
        .expect("owned metadata parses");
        assert!(config.features.harness_mode);
    }

    #[test]
    fn owned_script_metadata_replaces_by_name_config() {
        let plain = conformance_config_for_script("gc_block_allocations.luau");
        assert!(!plain.features.harness_mode);
        let parsed = conformance_config_for_script_source(
            "gc_block_allocations.luau",
            include_bytes!("../conformance-ruau/gc_block_allocations.luau"),
            ConformanceScriptOrigin::RuauOwned,
        )
        .expect("owned metadata parses");
        assert!(parsed.features.harness_mode);

        let plain = conformance_config_for_script("integer_regspill.luau");
        assert!(!plain.compile_options.fast_flag("LuauIntegerType"));
        let parsed = conformance_config_for_script_source(
            "integer_regspill.luau",
            include_bytes!("../conformance-ruau/integer_regspill.luau"),
            ConformanceScriptOrigin::RuauOwned,
        )
        .expect("owned metadata parses");
        assert!(parsed.compile_options.fast_flag("LuauIntegerType"));

        let plain = conformance_config_for_script("gc_basics.luau");
        assert!(!plain.runtime_compilation);
        let parsed = conformance_config_for_script_source(
            "gc_basics.luau",
            include_bytes!("../conformance-ruau/gc_basics.luau"),
            ConformanceScriptOrigin::RuauOwned,
        )
        .expect("owned metadata parses");
        assert!(parsed.runtime_compilation);

        let plain = conformance_config_for_script("pcall_oom_profile.luau");
        assert_eq!(plain.limits.max_memory_bytes, None);
        let parsed = conformance_config_for_script_source(
            "pcall_oom_profile.luau",
            include_bytes!("../conformance-ruau/pcall_oom_profile.luau"),
            ConformanceScriptOrigin::RuauOwned,
        )
        .expect("owned metadata parses");
        assert_eq!(parsed.limits.max_memory_bytes, Some(1 << 20));

        let plain = conformance_config_for_script("tables_sparse_boundary.luau");
        assert_eq!(plain.limits.gas, Some(CONFORMANCE_GAS));
        let parsed = conformance_config_for_script_source(
            "tables_sparse_boundary.luau",
            include_bytes!("../conformance-ruau/tables_sparse_boundary.luau"),
            ConformanceScriptOrigin::RuauOwned,
        )
        .expect("owned metadata parses");
        assert_eq!(
            parsed.limits.gas,
            Some(CONFORMANCE_TABLES_SPARSE_BOUNDARY_GAS)
        );

        let plain = conformance_config_for_script("coroutine_preemptible_resume.luau");
        assert_eq!(plain.limits.quantum, None);
        let parsed = conformance_config_for_script_source(
            "coroutine_preemptible_resume.luau",
            include_bytes!("../conformance-ruau/coroutine_preemptible_resume.luau"),
            ConformanceScriptOrigin::RuauOwned,
        )
        .expect("owned metadata parses");
        assert_eq!(parsed.limits.quantum, Some(25));

        let plain = conformance_config_for_script("require_module_source.luau");
        assert!(!plain.module_source);
        let parsed = conformance_config_for_script_source(
            "require_module_source.luau",
            include_bytes!("../conformance-ruau/require_module_source.luau"),
            ConformanceScriptOrigin::RuauOwned,
        )
        .expect("owned metadata parses");
        assert!(parsed.module_source);
    }

    #[test]
    fn ruau_owned_conformance_headers_are_machine_auditable() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance-ruau");
        let mut scripts = std::fs::read_dir(&dir)
            .expect("read conformance-ruau")
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("luau"))
            .collect::<Vec<_>>();
        scripts.sort();
        assert!(
            !scripts.is_empty(),
            "owned conformance suite must not be empty"
        );

        for path in scripts {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap();
            let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!("read {}: {error}", path.display());
            });
            let header = text
                .lines()
                .take_while(|line| line.trim().is_empty() || line.starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n");

            for marker in [
                "-- Ruau-owned conformance script.",
                "-- Source:",
                "-- Omitted",
                "-- Execution features:",
                "Compiler flags:",
                "RuntimeCapabilities:",
                "-- Conformance-only limits:",
            ] {
                assert!(
                    header.contains(marker),
                    "{name} missing header marker {marker}"
                );
            }
            assert!(
                !header.contains("remain to be decomposed separately"),
                "{name} still has stale decomposition prose"
            );
            assert!(
                !header.contains("belongs to yieldable-continuation work"),
                "{name} still points completed pcall OOM coverage at future work"
            );

            let config = conformance_config_for_script_source(
                name,
                text.as_bytes(),
                ConformanceScriptOrigin::RuauOwned,
            )
            .unwrap_or_else(|error| panic!("{name} metadata did not parse: {error}"));
            if header.contains("Execution features: harness mode") {
                assert!(
                    config.features.harness_mode,
                    "{name} header/config mismatch"
                );
            }
            if header.contains("runtime compilation") {
                assert!(config.runtime_compilation, "{name} header/config mismatch");
            }
            if header.contains("Compiler flags: LuauIntegerType") {
                assert!(
                    config.compile_options.fast_flag("LuauIntegerType"),
                    "{name} header/config mismatch"
                );
            }
            if header.contains("max memory bytes = 1 MiB") {
                assert_eq!(config.limits.max_memory_bytes, Some(1 << 20), "{name}");
            }
            if header.contains("CONFORMANCE_TABLES_SPARSE_BOUNDARY_GAS") {
                assert_eq!(
                    config.limits.gas,
                    Some(CONFORMANCE_TABLES_SPARSE_BOUNDARY_GAS),
                    "{name}"
                );
            }
        }
    }

    /// Reads a global by name off the main thread's global table. Interning is
    /// idempotent, so `intern_str` returns the existing handle for an installed
    /// name and lets us key the table; an absent name resolves to `nil`.
    fn global_value(vm: &mut Vm, name: &[u8]) -> RawValue {
        let globals = vm.main_thread().unwrap().globals.expect("globals");
        let Some(key) = vm.heap_mut().intern_str(name) else {
            return RawValue::Nil;
        };
        vm.heap()
            .table(globals)
            .map_or(RawValue::Nil, |t| t.get(RawValue::String(key)))
    }

    #[test]
    fn runtime_capabilities_install_only_their_selected_libraries() {
        // The default capability set installs every library.
        let mut full = test_vm();
        for library in Library::ALL {
            assert!(
                matches!(
                    global_value(&mut full, library.global_name_bytes()),
                    RawValue::Table(_)
                ),
                "default capabilities should install {library:?}"
            );
        }

        // An empty library set installs no library tables, but keeps base globals.
        let mut base = Vm::builder()
            .runtime_capabilities(
                RuntimeCapabilities::from_libraries([]).enable_runtime_compilation(),
            )
            .build_for_test();
        assert!(!base.is_poisoned());
        for library in Library::ALL {
            assert_eq!(
                global_value(&mut base, library.global_name_bytes()),
                RawValue::Nil,
                "base_only should omit {library:?}"
            );
        }
        assert!(matches!(
            global_value(&mut base, b"print"),
            RawValue::Function(_)
        ));
        assert!(matches!(
            global_value(&mut base, b"loadstring"),
            RawValue::Function(_)
        ));
        assert_eq!(global_value(&mut base, b"require"), RawValue::Nil);

        let mut no_runtime_compile = Vm::builder()
            .runtime_capabilities(RuntimeCapabilities::default())
            .build_for_test();
        assert_eq!(
            global_value(&mut no_runtime_compile, b"loadstring"),
            RawValue::Nil
        );
        assert!(matches!(
            global_value(&mut no_runtime_compile, b"math"),
            RawValue::Table(_)
        ));

        // A targeted capability set installs exactly what it selects.
        let mut math_only = Vm::builder()
            .runtime_capabilities(
                RuntimeCapabilities::from_libraries([Library::Math]).enable_runtime_compilation(),
            )
            .build_for_test();
        assert!(matches!(
            global_value(&mut math_only, b"math"),
            RawValue::Table(_)
        ));
        assert_eq!(global_value(&mut math_only, b"os"), RawValue::Nil);
    }

    #[test]
    fn omitting_a_library_makes_it_unreachable_at_runtime() {
        // A script reaching a disabled library fails closed (indexing nil), and the
        // VM is not poisoned by dropping coroutine/table (the prelude is skipped).
        let chunk = compile_source(
            "return pcall(function() return os.time() end)",
            &CompileOptions::default(),
            None,
        )
        .expect("compile");
        let mut vm = Vm::builder()
            .runtime_capabilities(
                RuntimeCapabilities::from_libraries([]).enable_runtime_compilation(),
            )
            .build_for_test();
        assert!(!vm.is_poisoned());
        let module = vm.load(&chunk).expect("load");
        // pcall returns (false, error message) when os.time indexes nil.
        assert!(matches!(
            vm.call(&module, Default::default())
                .expect("run")
                .as_slice(),
            [RawValue::Boolean(false), _]
        ));
    }

    #[test]
    fn a_contained_panic_poisons_the_vm() {
        let mut vm = test_vm();
        // A panic at the host-call boundary is contained as an error, not a crash.
        let result = vm.contained::<Vec<RawValue>>(|_, _| panic!("simulated VM bug"));
        assert!(matches!(
            result.expect_err("panic is surfaced as an unwind").kind,
            RuntimeErrorKind::PanicPoison
        ));
        assert!(vm.is_poisoned());
        // A poisoned VM refuses further work rather than touch a possibly
        // inconsistent heap.
        let again = vm.contained::<Vec<RawValue>>(|_, _| Ok(Vec::new()));
        assert!(matches!(
            again.expect_err("poisoned VM refuses reuse").kind,
            RuntimeErrorKind::PanicPoison
        ));
    }

    #[test]
    fn same_seed_vms_get_distinct_heap_nonces_and_reject_each_others_handles() {
        // Two VMs built from the same deterministic seed must not share a heap
        // identity, so a handle minted by one is rejected by the other (§6.2).
        let seed = Ambient::deterministic(0);
        let mut a = Vm::builder().ambient(seed).build_for_test();
        let b = Vm::builder().ambient(seed).build_for_test();
        assert_ne!(a.heap().id, b.heap().id);

        let handle = a.heap_mut().alloc_table(LuaTable::new()).expect("alloc");
        assert!(a.heap().table(handle).is_some(), "valid in its own heap");
        assert!(
            b.heap().table(handle).is_none(),
            "a foreign heap rejects the handle"
        );
    }

    #[test]
    fn hash_seed_drives_table_and_interner_hashers() {
        fn seeded_heap(seed: u64) -> Heap {
            Heap::new(HeapId(seed + 1), Ambient::deterministic(seed).config)
        }

        let mut a = seeded_heap(11);
        let mut b = seeded_heap(11);
        let mut c = seeded_heap(12);
        let key_a = a.intern_str(b"tenant-key").expect("intern a");
        let key_b = b.intern_str(b"tenant-key").expect("intern b");
        let key_c = c.intern_str(b"tenant-key").expect("intern c");
        let table_a = a.alloc_table(LuaTable::new()).expect("table a");
        let table_b = b.alloc_table(LuaTable::new()).expect("table b");
        let table_c = c.alloc_table(LuaTable::new()).expect("table c");

        let table_hash_a = a
            .table(table_a)
            .and_then(|table| table.hash_for_key(RawValue::String(key_a)))
            .expect("table hash a");
        let table_hash_b = b
            .table(table_b)
            .and_then(|table| table.hash_for_key(RawValue::String(key_b)))
            .expect("table hash b");
        let table_hash_c = c
            .table(table_c)
            .and_then(|table| table.hash_for_key(RawValue::String(key_c)))
            .expect("table hash c");

        assert_eq!(
            a.interner.hash_for(b"tenant-key"),
            b.interner.hash_for(b"tenant-key")
        );
        assert_ne!(
            a.interner.hash_for(b"tenant-key"),
            c.interner.hash_for(b"tenant-key")
        );
        assert_eq!(table_hash_a, table_hash_b);
        assert_ne!(table_hash_a, table_hash_c);
    }

    #[test]
    fn table_iteration_order_is_stable_across_hash_seeds() {
        fn table_order(seed: u64) -> Vec<Vec<u8>> {
            let mut heap = Heap::new(HeapId(seed + 10), Ambient::deterministic(seed).config);
            let keys = [
                b"zeta".as_slice(),
                b"alpha".as_slice(),
                b"middle".as_slice(),
            ];
            let handles = keys.map(|key| heap.intern_str(key).expect("intern key"));
            let table = heap.alloc_table(LuaTable::new()).expect("table");
            for (index, handle) in handles.into_iter().enumerate() {
                heap.table_mut(table)
                    .expect("table")
                    .set(RawValue::String(handle), RawValue::Number(index as f64));
            }

            let mut out = Vec::new();
            let mut cursor = RawValue::Nil;
            while let table::NextStep::Pair(key, _) = heap.table(table).expect("table").next(cursor)
            {
                let RawValue::String(handle) = key else {
                    panic!("expected string key, got {key:?}");
                };
                out.push(heap.string(handle).expect("string").bytes().to_vec());
                cursor = key;
            }
            out
        }

        assert_eq!(table_order(1), table_order(2));
    }

    #[test]
    fn keyed_hashing_keeps_memory_accounting_seed_independent() {
        fn footprint_after_work(seed: u64) -> usize {
            let mut heap = Heap::new(HeapId(seed + 20), Ambient::deterministic(seed).config);
            let baseline = heap.total_bytes();
            let table = heap.alloc_table(LuaTable::new()).expect("table");
            for index in 0..32 {
                let name = format!("key-{index}");
                let key = heap.intern_str(name.as_bytes()).expect("intern key");
                heap.table_mut(table)
                    .expect("table")
                    .set(RawValue::String(key), RawValue::Number(index as f64));
            }
            heap.total_bytes() - baseline
        }

        assert_eq!(footprint_after_work(100), footprint_after_work(200));
    }

    /// A default-capability [`CompiledModule`] artifact for the compile-once,
    /// instantiate-many tests.
    fn artifact(source: &str) -> CompiledModule {
        let chunk = compile_source(source, &CompileOptions::default(), None).expect("compile");
        CompiledModule::new(chunk, RuntimeCapabilities::default()).expect("a fresh chunk validates")
    }

    #[test]
    fn one_artifact_feeds_many_vms_with_independent_deterministic_ambients() {
        // The fleet shape: one artifact, N VMs, each with its own ambient seed.
        // Determinism must hold per VM (same seed → same result) while seeds
        // stay independent (the shared artifact leaks no PRNG state between
        // instances).
        let module = artifact("local a = math.random(2^30)\nreturn a, math.random(2^30)");
        let run = |seed: u64| {
            let mut vm = Vm::builder()
                .ambient(Ambient::deterministic(seed))
                .build_for_test();
            let loaded = vm.load_compiled(&module).expect("artifact loads");
            vm.call(&loaded, Default::default()).expect("artifact runs")
        };
        assert_eq!(
            run(1),
            run(1),
            "same seed reproduces from a shared artifact"
        );
        assert_eq!(run(2), run(2));
        assert_ne!(run(1), run(2), "distinct seeds stay independent");
    }

    #[test]
    fn load_compiled_rejects_a_runtime_capabilities_mismatch() {
        // Fail closed in both directions: an artifact compiled under a
        // narrower capability set must not load into a wider VM (its suppressed
        // folds assume the library is absent), and vice versa.
        let chunk = compile_source("return 1", &CompileOptions::default(), None).expect("compile");
        let narrow = RuntimeCapabilities::from_libraries(
            Library::ALL
                .into_iter()
                .filter(|library| *library != Library::Math),
        );
        let module = CompiledModule::new(chunk, narrow.clone()).expect("artifact validates");
        let mut wide_vm = test_vm();
        assert_eq!(
            wide_vm.load_compiled(&module).err(),
            Some(LoadError::RuntimeCapabilitiesMismatch {
                artifact: narrow.clone(),
                vm: RuntimeCapabilities::default(),
            })
        );
        // The matching capabilities load.
        let mut narrow_vm = Vm::builder().runtime_capabilities(narrow).build_for_test();
        assert!(narrow_vm.load_compiled(&module).is_ok());
    }

    #[test]
    fn preload_fails_the_build_closed_on_a_runtime_capabilities_mismatch() {
        let chunk = compile_source("return 1", &CompileOptions::default(), None).expect("compile");
        let narrow = RuntimeCapabilities::from_libraries(
            Library::ALL
                .into_iter()
                .filter(|library| *library != Library::Os),
        );
        let module = CompiledModule::new(chunk, narrow.clone()).expect("artifact validates");
        let error = Vm::builder()
            .ambient(Ambient::deterministic(0))
            .limits(Limits::unlimited())
            .runtime_capabilities(RuntimeCapabilities::default())
            .preload(&module)
            .trusted_host()
            .build()
            .err();
        assert_eq!(
            error,
            Some(VmBuildError::Preload(
                LoadError::RuntimeCapabilitiesMismatch {
                    artifact: narrow,
                    vm: RuntimeCapabilities::default(),
                },
            )),
            "a mismatched preload artifact must fail the build, not hand back a VM"
        );
    }

    #[test]
    fn preload_instantiates_at_build_and_take_preloaded_drains_once() {
        let first = artifact("return 1");
        let second = artifact("return 2");
        let mut vm = Vm::builder()
            .ambient(Ambient::deterministic(0))
            .limits(Limits::unlimited())
            .runtime_capabilities(RuntimeCapabilities::default())
            .preload(&first)
            .preload(&second)
            .trusted_host()
            .build()
            .expect("preloads instantiate");
        let modules = vm.take_preloaded();
        assert_eq!(modules.len(), 2, "registration order, one module each");
        assert!(matches!(
            vm.call(&modules[0], Default::default()).as_deref(),
            Ok([RawValue::Number(value)]) if *value == 1.0
        ));
        assert!(matches!(
            vm.call(&modules[1], Default::default()).as_deref(),
            Ok([RawValue::Number(value)]) if *value == 2.0
        ));
        assert!(vm.take_preloaded().is_empty(), "drained once");
        for module in modules {
            vm.unload(module);
        }
    }

    #[test]
    fn artifact_and_vms_drop_in_either_order() {
        // Predeceasing: the artifact can drop before the VMs that loaded from
        // it — each VM owns its instantiated proto graph outright.
        let module = artifact("return 40 + 2");
        let mut vm = test_vm();
        let loaded = vm.load_compiled(&module).expect("load");
        drop(module);
        assert!(matches!(
            vm.call(&loaded, Default::default()).as_deref(),
            Ok([RawValue::Number(value)]) if *value == 42.0
        ));

        // Outliving: the artifact stays loadable after every VM built from it
        // is gone (clones share one immutable chunk).
        let module = artifact("return 40 + 2");
        let clone = module.clone();
        let mut vm = test_vm();
        let loaded = vm.load_compiled(&module).expect("load");
        let _ = vm.call(&loaded, Default::default()).expect("run");
        drop(vm);
        drop(module);
        let mut next = test_vm();
        let loaded = next
            .load_compiled(&clone)
            .expect("the artifact outlives the fleet");
        assert!(matches!(
            next.call(&loaded, Default::default()).as_deref(),
            Ok([RawValue::Number(value)]) if *value == 42.0
        ));
    }

    #[test]
    fn repeated_load_compiled_into_one_vm_yields_independent_modules() {
        let module = artifact("counter = (counter or 0) + 1\nreturn counter");
        let mut vm = test_vm();
        let first = vm.load_compiled(&module).expect("first load");
        let second = vm.load_compiled(&module).expect("second load");
        // Two independent instantiations of one artifact in one VM: both
        // callable, sharing the VM's globals like any two loaded modules.
        assert!(matches!(
            vm.call(&first, Default::default()).as_deref(),
            Ok([RawValue::Number(value)]) if *value == 1.0
        ));
        assert!(matches!(
            vm.call(&second, Default::default()).as_deref(),
            Ok([RawValue::Number(value)]) if *value == 2.0
        ));
        vm.unload(first);
        // Unloading one instance leaves the other callable.
        assert!(matches!(
            vm.call(&second, Default::default()).as_deref(),
            Ok([RawValue::Number(value)]) if *value == 3.0
        ));
        vm.unload(second);
    }

    /// Compiles and loads `source` under `chunk_name`, for the traceback tests.
    fn load_text(vm: &mut Vm, chunk_name: &[u8], source: &str) -> LoadedModule {
        let chunk = compile_source(source, &CompileOptions::default(), None).expect("compile");
        vm.load_named(&chunk, chunk_name).expect("load")
    }

    /// The multi-frame erroring script the traceback-frame tests share.
    const TRACEBACK_SCRIPT: &str = "local function inner()\n    error(\"boom\")\nend\nlocal function outer()\n    inner()\nend\nouter()\n";

    /// The structured frames `TRACEBACK_SCRIPT` fails with under chunk name `tb`.
    fn traceback_script_frames() -> Vec<TracebackFrame> {
        vec![
            TracebackFrame {
                chunk_name: "tb".to_owned(),
                line: Some(2),
                function_name: Some("inner".to_owned()),
            },
            TracebackFrame {
                chunk_name: "tb".to_owned(),
                line: Some(5),
                function_name: Some("outer".to_owned()),
            },
            TracebackFrame {
                chunk_name: "tb".to_owned(),
                line: Some(7),
                function_name: None,
            },
        ]
    }

    /// Re-renders structured frames the way the engine renders traceback text,
    /// so the tests can assert the text is derived from the frames.
    fn render_frames(frames: &[TracebackFrame]) -> String {
        frames
            .iter()
            .map(|frame| {
                let mut line = frame.chunk_name.clone();
                if let Some(number) = frame.line {
                    line.push_str(&format!(":{number}"));
                }
                if let Some(name) = &frame.function_name {
                    line.push_str(&format!(" function {name}"));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn protected_error_traceback_frames_match_the_rendered_text() {
        let mut vm = test_vm();
        let module = load_text(&mut vm, b"=tb", TRACEBACK_SCRIPT);
        let error = vm
            .call_protected(&module, Default::default())
            .expect("catchable")
            .expect_err("the script raises");
        assert_eq!(error.frames(), traceback_script_frames().as_slice());
        assert!(!error.frames_truncated());
        // The rendered text is derived from the frames, line for line.
        assert_eq!(
            error.traceback(),
            Some("tb:2 function inner\ntb:5 function outer\ntb:7")
        );
        assert_eq!(
            error.traceback(),
            Some(render_frames(error.frames()).as_str())
        );
    }

    #[test]
    fn source_load_name_drives_traceback_frame_names() {
        let source =
            ruau_source::Source::text(ModuleId::new("tracebacks/source.luau"), TRACEBACK_SCRIPT);
        let chunk = compile_source(
            source.as_str().expect("traceback source is UTF-8"),
            &CompileOptions::default(),
            None,
        )
        .expect("compile");
        let mut vm = test_vm();
        let module = vm
            .load_named(&chunk, &source.load_name())
            .expect("load source");
        let error = vm
            .call_protected(&module, Default::default())
            .expect("catchable")
            .expect_err("the script raises");

        assert_eq!(error.frames()[0].chunk_name, "tracebacks/source.luau");
        assert_eq!(
            error.traceback(),
            Some(
                "tracebacks/source.luau:2 function inner\n\
                 tracebacks/source.luau:5 function outer\n\
                 tracebacks/source.luau:7"
            )
        );
    }

    #[test]
    fn traceback_frame_collection_honors_the_byte_budget() {
        let mut vm = test_vm();
        let module = load_text(&mut vm, b"=tb", TRACEBACK_SCRIPT);
        let main_id = vm.main_thread;
        let mut thread = vm.heap.take_thread(main_id).expect("take main thread");
        // "tb:2 function inner" is 19 bytes; a 30-byte budget fits it whole,
        // then cuts the second frame's line mid-render.
        let failure =
            call::run_protected_with_traceback(&mut vm.heap, &mut thread, module.main, 30)
                .expect("protected run")
                .expect_err("the script raises");
        assert_eq!(
            failure.traceback.as_deref(),
            Some("tb:2 function inner\ntb:5 funct"),
            "the text keeps the historical byte-budgeted prefix"
        );
        // The structured capture drops the cut frame whole and marks the cut.
        let capture = thread
            .captured_traceback
            .take()
            .expect("the unwind stashes the structured capture");
        let error = ProtectedScriptError::from_failure(&mut vm.heap, failure, Some(capture));
        assert_eq!(error.frames(), &traceback_script_frames()[..1]);
        assert!(error.frames_truncated());
        assert_eq!(error.traceback(), Some("tb:2 function inner\ntb:5 funct"));

        // A stale capture is never inherited: a failure whose traceback text
        // does not match the stash gets no frames.
        let stale = call::run_protected_with_traceback(&mut vm.heap, &mut thread, module.main, 30)
            .expect("protected run")
            .expect_err("the script raises");
        let stale_capture = thread.captured_traceback.take();
        let mut mismatched = stale;
        mismatched.traceback = Some("a different rendering".to_owned());
        let error = ProtectedScriptError::from_failure(&mut vm.heap, mismatched, stale_capture);
        assert!(error.frames().is_empty());
        assert!(!error.frames_truncated());

        assert!(vm.heap.put_thread(main_id, thread));
    }

    #[test]
    fn host_raised_error_frames_point_at_the_lua_call_site() {
        fn boomhost(_scope: &Scope<'_>, (): ()) -> Result<(), scope::RuntimeError> {
            Err(scope::RuntimeError::runtime("host boom"))
        }

        let mut vm = test_vm();
        let globals = vm.main_thread().unwrap().globals.expect("globals");
        let closure = vm
            .heap_mut()
            .alloc_scoped_host(scoped_host_fn(boomhost))
            .expect("alloc scoped host");
        let key = vm.heap_mut().intern_str(b"boomhost").expect("intern name");
        vm.heap_mut()
            .table_mut(globals)
            .expect("globals table")
            .set(RawValue::String(key), RawValue::Function(closure));
        let module = load_text(
            &mut vm,
            b"=hosttb",
            "local function call_host()\n    boomhost()\nend\ncall_host()\n",
        );
        let error = vm
            .call_protected(&module, Default::default())
            .expect("catchable")
            .expect_err("the host function raises");
        // A host activation occupies no Lua frame: the innermost frame is the
        // Lua call site of the host function, not a synthetic native frame.
        assert_eq!(
            error.frames(),
            &[
                TracebackFrame {
                    chunk_name: "hosttb".to_owned(),
                    line: Some(2),
                    function_name: Some("call_host".to_owned()),
                },
                TracebackFrame {
                    chunk_name: "hosttb".to_owned(),
                    line: Some(4),
                    function_name: None,
                },
            ]
        );
        assert_eq!(
            error.traceback(),
            Some(render_frames(error.frames()).as_str())
        );
    }

    #[tokio::test]
    async fn marshaled_script_error_carries_the_structured_frames() {
        let mut vm = test_vm();
        let module = load_text(&mut vm, b"=tb", TRACEBACK_SCRIPT);
        let ExecError::Script(error) = vm
            .exec_async(&module, Default::default())
            .await
            .expect_err("the script raises")
        else {
            panic!("expected catchable script error");
        };
        assert_eq!(error.frames(), traceback_script_frames().as_slice());
        assert!(!error.frames_truncated());
        assert_eq!(
            error.traceback(),
            Some("tb:2 function inner\ntb:5 function outer\ntb:7")
        );
    }
}
