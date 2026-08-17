use super::{Scope, ScopedValue};
use crate::{
    TracebackFrame,
    api::{HostPayload, OwnedValue, RuntimeErrorKind, ScriptErrorField, Unwind},
    debug,
};

/// A host-facing error raised from a [`Scope`] step (or, later, a host function),
/// surfaced to Luau as a catchable runtime error.
///
/// Unlike the engine's internal [`Unwind`](crate::api::Unwind) control-flow
/// carrier, this is the *public* error a host returns: it owns a plain message and
/// a [`RuntimeErrorKind`], names no heap value, and implements
/// [`std::error::Error`], so it composes with `?` in host code.
#[derive(Clone, Debug)]
pub struct RuntimeError {
    message: String,
    kind: RuntimeErrorKind,
    /// Typed host freight riding this error to the host's exit surfaces;
    /// scripts never observe it. See [`RuntimeError::with_payload`].
    payload: Option<HostPayload>,
    script_fields: Vec<ScriptErrorField>,
}

impl RuntimeError {
    /// An error originating *outside* the VM — a host capability, I/O, or
    /// conversion failure. Catchable by `pcall`, like an ordinary runtime error.
    pub fn external(message: impl std::fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            kind: RuntimeErrorKind::Runtime,
            payload: None,
            script_fields: Vec::new(),
        }
    }

    /// A runtime error equivalent to a Lua `error(message)`. Catchable by `pcall`.
    pub fn runtime(message: impl std::fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            kind: RuntimeErrorKind::Runtime,
            payload: None,
            script_fields: Vec::new(),
        }
    }

    /// An out-of-memory error: an allocation or pin would exceed the VM's memory
    /// cap. Carries [`RuntimeErrorKind::Memory`] so the runner accounts it
    /// distinctly from an ordinary runtime error.
    pub fn memory(message: impl std::fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            kind: RuntimeErrorKind::Memory,
            payload: None,
            script_fields: Vec::new(),
        }
    }

    /// The error a poisoned VM returns when it refuses further work. Fatal
    /// ([`RuntimeErrorKind::PanicPoison`]): a poisoned VM must be dropped, not reused.
    pub(crate) fn poisoned() -> Self {
        Self {
            message: "the VM is poisoned after a contained panic and refuses further work"
                .to_string(),
            kind: RuntimeErrorKind::PanicPoison,
            payload: None,
            script_fields: Vec::new(),
        }
    }

    /// Builds an embedding-surface error from an existing VM failure category.
    pub(crate) fn with_kind(message: impl Into<String>, kind: RuntimeErrorKind) -> Self {
        Self {
            message: message.into(),
            kind,
            payload: None,
            script_fields: Vec::new(),
        }
    }

    /// Maps a nested-call [`Unwind`] to a host-facing error, **preserving the
    /// failure kind** so the runner's typed metrics stay accurate. The message
    /// names only the failure category; the error value's text is not
    /// rendered.
    pub(crate) fn from_unwind(unwind: &Unwind) -> Self {
        let message = match unwind.kind {
            RuntimeErrorKind::Memory => "Scope::call: the called function exceeded the memory cap",
            RuntimeErrorKind::Cancelled => "Scope::call: the called function was cancelled",
            _ => "Scope::call: the called function raised an error",
        };
        Self {
            message: message.to_string(),
            kind: unwind.kind,
            payload: None,
            script_fields: Vec::new(),
        }
    }

    pub(super) fn from_uncatchable_protected_kind(kind: RuntimeErrorKind) -> Self {
        let message = match kind {
            RuntimeErrorKind::Cancelled => {
                "Scope::call_protected: the called function was cancelled"
            }
            RuntimeErrorKind::Deadline => {
                "Scope::call_protected: the called function exceeded its deadline"
            }
            RuntimeErrorKind::PanicPoison => {
                "Scope::call_protected: the VM is poisoned and refuses further work"
            }
            _ => "Scope::call_protected: an uncatchable error escaped the protected call",
        };
        Self {
            message: message.to_string(),
            kind,
            payload: None,
            script_fields: Vec::new(),
        }
    }

    /// The human-facing message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The failure category carried to the runner's typed metrics when this error
    /// is raised.
    #[must_use]
    pub fn kind(&self) -> RuntimeErrorKind {
        self.kind
    }

    /// Prefixes this error with a nested conversion path while preserving its
    /// failure category.
    #[must_use]
    pub fn with_path(mut self, path: impl std::fmt::Display) -> Self {
        self.message = format!("at {path}: {}", self.message);
        self
    }

    /// Builds an error whose script-visible value is a table.
    ///
    /// The engine always writes the human-facing [`message`](Self::message) into
    /// the table's `message` field, then applies these fields in order. A caller
    /// that supplies its own `message` field deliberately overrides the canonical
    /// value.
    pub fn structured(
        message: impl std::fmt::Display,
        fields: impl IntoIterator<Item = ScriptErrorField>,
    ) -> Self {
        Self::runtime(message).with_script_fields(fields)
    }

    /// Adds one script-visible field to this error's table representation.
    ///
    /// Field values are owned host values; registry pins are materialized and
    /// released by the engine at the same boundary as host returns.
    #[must_use]
    pub fn with_script_field(
        mut self,
        name: impl Into<std::borrow::Cow<'static, str>>,
        value: impl Into<OwnedValue>,
    ) -> Self {
        self.script_fields.push(ScriptErrorField::new(name, value));
        self
    }

    /// Adds script-visible fields to this error's table representation.
    #[must_use]
    pub fn with_script_fields(
        mut self,
        fields: impl IntoIterator<Item = ScriptErrorField>,
    ) -> Self {
        self.script_fields.extend(fields);
        self
    }

    /// Script-visible fields that make this error materialize as a Lua table.
    #[must_use]
    pub fn script_fields(&self) -> &[ScriptErrorField] {
        &self.script_fields
    }

    /// Attaches typed, host-only freight to this error. The payload rides the
    /// error through raise → script `pcall` → re-raise → unwind, and is
    /// recovered with `payload_ref` on this type, [`ScriptError`],
    /// `ProtectedScriptError`, and `MarshaledScriptError`. Script-visible
    /// behavior is unchanged: the script sees exactly the message string a
    /// payload-less raise would produce.
    ///
    /// # Design: where the payload lives
    ///
    /// Once raised, an error *is* a Lua value, and the error representation
    /// (`RawValue`) cannot be extended without a script-visible change — so the
    /// payload is not part of the value. Instead it travels in two stages:
    ///
    /// - **In flight**, the payload rides the engine's internal raised-error
    ///   carrier from the host raise to the first boundary that materializes
    ///   the error into a Lua value (a `pcall` catch, or the exit unwind).
    /// - **Across a script catch**, the engine parks the payload in a VM-side
    ///   table keyed by the *identity* of the materialized error value (its
    ///   generational heap handle). A re-raise of that same value reaches the
    ///   exit boundary with the same identity, and the surface recovers the
    ///   payload by lookup.
    ///
    /// # Loss semantics
    ///
    /// Preservation follows value identity. A scoped host error materializes
    /// as an interned string, and interning makes string identity *content*
    /// equality, so:
    ///
    /// - `error(caught, 0)` re-raises the caught value unchanged — the payload
    ///   **survives** (as does raising any byte-equal string, an interning
    ///   artifact).
    /// - `error(caught)` prefixes a fresh `source:line:` onto the message,
    ///   producing a *different* string — the payload **drops**. The same
    ///   applies to `tostring`, concatenation, and any other rewrap that
    ///   builds a new value, and to raising an unrelated error (no smuggling).
    /// - A payload survives only within the run that raised it: the table is a
    ///   small fixed-capacity FIFO, so a script that catches and discards many
    ///   payload-carrying errors evicts the oldest entries.
    ///
    /// Tracked entries hold no GC root: the script's own reference (the caught
    /// local) keeps the error value alive across a collection, and generational
    /// handles make an entry for a collected value inert rather than
    /// re-attachable. Fatal kinds (`Cancelled`/`Deadline`/`PanicPoison`) are
    /// unaffected by payload presence: they stay uncatchable, and the payload
    /// simply rides to the outer error.
    ///
    /// # Sharing
    ///
    /// The payload is `Arc`-shared: cloning the error or re-raising it clones a
    /// reference to the same payload value (clone-on-rethrow never deep-copies),
    /// and `payload_ref` borrows it in place.
    #[must_use]
    pub fn with_payload(mut self, payload: impl std::any::Any + Send + Sync) -> Self {
        self.payload = Some(HostPayload::new(payload));
        self
    }

    /// The typed payload attached with [`with_payload`](Self::with_payload),
    /// downcast to `T`; `None` if no payload is attached or `T` is not the
    /// attached type.
    #[must_use]
    pub fn payload_ref<T: std::any::Any>(&self) -> Option<&T> {
        self.payload.as_ref().and_then(HostPayload::downcast_ref)
    }

    pub(crate) fn into_error_parts(
        self,
    ) -> (
        String,
        RuntimeErrorKind,
        Option<HostPayload>,
        Vec<ScriptErrorField>,
    ) {
        (self.message, self.kind, self.payload, self.script_fields)
    }

    /// Re-attaches an untyped payload handle recovered at an engine boundary.
    pub(crate) fn with_host_payload(mut self, payload: Option<HostPayload>) -> Self {
        self.payload = payload;
        self
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// A script error caught by [`Scope::call_protected`].
///
/// The error value is scope-branded: it can be inspected during the current
/// [`Scope`] step, and a host that wants to keep a heap value must copy or stash
/// it before making another nested call.
#[derive(Clone, Debug)]
pub struct ScriptError<'s> {
    value: ScopedValue<'s>,
    kind: RuntimeErrorKind,
    traceback: Option<String>,
    frames: Vec<TracebackFrame>,
    frames_truncated: bool,
    payload: Option<HostPayload>,
}

impl<'s> ScriptError<'s> {
    #[must_use]
    pub(super) fn new(value: ScopedValue<'s>, kind: RuntimeErrorKind) -> Self {
        Self {
            value,
            kind,
            traceback: None,
            frames: Vec::new(),
            frames_truncated: false,
            payload: None,
        }
    }

    pub(super) fn with_traceback(
        mut self,
        traceback: Option<String>,
        capture: Option<debug::Traceback>,
    ) -> Self {
        let (frames, frames_truncated) = debug::frames_for_traceback(traceback.as_deref(), capture);
        self.traceback = traceback;
        self.frames = frames;
        self.frames_truncated = frames_truncated;
        self
    }

    pub(super) fn with_host_payload(mut self, payload: Option<HostPayload>) -> Self {
        self.payload = payload;
        self
    }

    /// The Lua error value surfaced by the protected call.
    #[must_use]
    pub fn value(&self) -> ScopedValue<'s> {
        self.value
    }

    /// The failure category carried to runner metrics.
    #[must_use]
    pub fn kind(&self) -> RuntimeErrorKind {
        self.kind
    }

    /// The captured traceback, if this protected-call path captured one.
    #[must_use]
    pub fn traceback(&self) -> Option<&str> {
        self.traceback.as_deref()
    }

    /// The structured frames of the captured traceback, innermost first.
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

    /// Whether the traceback byte budget cut frame collection short.
    #[must_use]
    pub fn frames_truncated(&self) -> bool {
        self.frames_truncated
    }

    /// A conservative display message for the Lua error value.
    ///
    /// String errors return their string bytes lossily decoded as UTF-8. Scalar
    /// values use Luau's scalar spelling. Heap objects return their type name;
    /// this accessor does not call `tostring` or run metamethods while handling
    /// an error.
    #[must_use]
    pub fn message(&self, scope: &Scope<'s>) -> String {
        self.value.display(scope)
    }

    /// The typed host payload riding the caught error, if the error was raised
    /// by a host function via [`RuntimeError::with_payload`] (directly, or
    /// re-raised by the script as the same error value). See that method for
    /// the preservation/loss semantics.
    #[must_use]
    pub fn payload_ref<T: std::any::Any>(&self) -> Option<&T> {
        self.payload.as_ref().and_then(HostPayload::downcast_ref)
    }
}
