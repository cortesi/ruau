//! Shared accessors for script-error surfaces across value representations.

use ruau_vm_api::RuntimeErrorKind;

use super::scope::ScriptError;
use crate::{MarshaledScriptError, ProtectedScriptError, TracebackFrame, host::HostScriptError};

/// Common read-only accessors for catchable script failures.
///
/// Each concrete error type keeps its own value representation and lifetime;
/// this trait documents and unifies the metadata every surface exposes.
pub trait ScriptErrorAccess {
    /// Failure category for metrics and policy.
    fn kind(&self) -> RuntimeErrorKind;
    /// Rendered traceback text, when captured.
    fn traceback(&self) -> Option<&str>;
    /// Structured traceback frames, innermost first.
    fn frames(&self) -> &[TracebackFrame];
    /// Typed host payload attached by [`RuntimeError::with_payload`](crate::scope::RuntimeError::with_payload).
    fn payload_ref<T: std::any::Any>(&self) -> Option<&T>;
}

impl<'s> ScriptErrorAccess for ScriptError<'s> {
    fn kind(&self) -> RuntimeErrorKind {
        ScriptError::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        ScriptError::traceback(self)
    }

    fn frames(&self) -> &[TracebackFrame] {
        &[]
    }

    fn payload_ref<T: std::any::Any>(&self) -> Option<&T> {
        ScriptError::payload_ref(self)
    }
}

impl ScriptErrorAccess for ProtectedScriptError {
    fn kind(&self) -> RuntimeErrorKind {
        Self::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        Self::traceback(self)
    }

    fn frames(&self) -> &[TracebackFrame] {
        Self::frames(self)
    }

    fn payload_ref<T: std::any::Any>(&self) -> Option<&T> {
        Self::payload_ref(self)
    }
}

impl ScriptErrorAccess for MarshaledScriptError {
    fn kind(&self) -> RuntimeErrorKind {
        Self::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        Self::traceback(self)
    }

    fn frames(&self) -> &[TracebackFrame] {
        Self::frames(self)
    }

    fn payload_ref<T: std::any::Any>(&self) -> Option<&T> {
        Self::payload_ref(self)
    }
}

impl ScriptErrorAccess for HostScriptError {
    fn kind(&self) -> RuntimeErrorKind {
        Self::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        Self::traceback(self)
    }

    fn frames(&self) -> &[TracebackFrame] {
        &[]
    }

    fn payload_ref<T: std::any::Any>(&self) -> Option<&T> {
        None
    }
}

impl ScriptErrorAccess for crate::ExecError {
    fn kind(&self) -> RuntimeErrorKind {
        Self::kind(self)
    }

    fn traceback(&self) -> Option<&str> {
        self.script_error()
            .and_then(MarshaledScriptError::traceback)
    }

    fn frames(&self) -> &[TracebackFrame] {
        self.script_error()
            .map(MarshaledScriptError::frames)
            .unwrap_or(&[])
    }

    fn payload_ref<T: std::any::Any>(&self) -> Option<&T> {
        self.script_error()
            .and_then(|error| MarshaledScriptError::payload_ref(error))
    }
}
