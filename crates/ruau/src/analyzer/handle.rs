//! Checker handle, cancellation, and busy-state lifecycle.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};

use super::AnalysisError;

/// A reusable cancellation token. Signal it from any thread to interrupt a
/// running check.
///
/// `CancellationToken` is `Send` and `Sync`: the underlying Luau implementation
/// manages signaled state through atomic operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    /// Shared token internals.
    inner: Arc<CancellationTokenInner>,
}

/// Shared cancellation token internals.
#[derive(Debug)]
struct CancellationTokenInner {
    /// Raw C cancellation token handle.
    raw: ffi::RuauTokenHandle,
}

// SAFETY: The underlying C cancellation token uses atomic state and is thread-safe for
// signal/reset. The handle itself is an opaque pointer that can be moved or shared across
// threads.
unsafe impl Send for CancellationTokenInner {}
// SAFETY: see Send impl above.
unsafe impl Sync for CancellationTokenInner {}

impl Drop for CancellationTokenInner {
    fn drop(&mut self) {
        // SAFETY: `raw` originates from `ruau_cancellation_token_new` and is valid until drop.
        unsafe { ffi::ruau_cancellation_token_free(self.raw) };
    }
}

impl CancellationToken {
    /// Creates a new cancellation token.
    pub fn new() -> Result<Self, AnalysisError> {
        // SAFETY: Calling into shim constructor. Null indicates failure.
        let raw = unsafe { ffi::ruau_cancellation_token_new() };
        if raw.is_null() {
            return Err(AnalysisError::CreateCancellationTokenFailed);
        }
        Ok(Self {
            inner: Arc::new(CancellationTokenInner { raw }),
        })
    }

    /// Requests cancellation on this token.
    pub fn cancel(&self) {
        // SAFETY: `raw` is valid while `inner` is alive.
        unsafe { ffi::ruau_cancellation_token_cancel(self.inner.raw) };
    }

    /// Clears cancellation state on this token.
    pub fn reset(&self) {
        // SAFETY: `raw` is valid while `inner` is alive.
        unsafe { ffi::ruau_cancellation_token_reset(self.inner.raw) };
    }

    /// Returns the raw C token handle.
    pub(super) fn raw(&self) -> ffi::RuauTokenHandle {
        self.inner.raw
    }
}

/// Native checker handle plus the in-flight busy flag.
///
/// Wrapping the native handle in an `Arc` lets the `spawn_blocking` closure outlive the user's
/// `&mut Checker` borrow: when the future is dropped before the closure finishes, the closure's
/// `Arc` clone keeps the handle alive until the C call returns. The `busy` flag prevents the
/// next operation from re-entering the same handle while a previous job is still draining.
pub(super) struct CheckerHandleInner {
    /// Opaque native checker handle. Freed in `Drop`.
    raw: ffi::RuauCheckerHandle,
    /// Set while a check is running on the blocking pool. `compare_exchange` claims the slot.
    busy: AtomicBool,
}

// SAFETY: The native checker is single-threaded for its operations, but the *handle* itself is
// just an opaque pointer and can move between threads. The busy flag and `Arc` together
// serialize access so only one operation touches the handle at a time.
unsafe impl Send for CheckerHandleInner {}
// SAFETY: see Send impl above.
unsafe impl Sync for CheckerHandleInner {}

impl CheckerHandleInner {
    /// Creates a checker handle wrapper with an unclaimed busy flag.
    pub(super) fn new(raw: ffi::RuauCheckerHandle) -> Self {
        Self {
            raw,
            busy: AtomicBool::new(false),
        }
    }

    /// Returns the raw native checker handle.
    pub(super) fn raw(&self) -> ffi::RuauCheckerHandle {
        self.raw
    }

    /// Clears the in-flight busy flag.
    pub(super) fn clear_busy(&self) {
        self.busy.store(false, AtomicOrdering::Release);
    }
}

impl Drop for CheckerHandleInner {
    fn drop(&mut self) {
        // SAFETY: `raw` originates from `ruau_checker_new` and is valid until drop.
        unsafe { ffi::ruau_checker_free(self.raw) };
    }
}

/// RAII guard that clears the `busy` flag on drop, including the panic path.
pub(super) struct BusyGuard(Arc<CheckerHandleInner>);

impl BusyGuard {
    /// Creates a guard for a busy checker handle.
    pub(super) fn new(handle: Arc<CheckerHandleInner>) -> Self {
        Self(handle)
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.clear_busy();
    }
}

/// Synchronously-claimed busy slot that releases on drop unless transferred via `into_arc`.
///
/// Lets `check_with_options` hold the busy flag across fallible setup work (input copy,
/// token allocation), and then move ownership into the `spawn_blocking` closure. Failure
/// before transfer drops the claim and clears the flag automatically.
pub(super) struct BusyClaim {
    handle: Arc<CheckerHandleInner>,
    armed: bool,
}

impl BusyClaim {
    pub(super) fn new(handle: Arc<CheckerHandleInner>) -> Result<Self, AnalysisError> {
        handle
            .busy
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .map_err(|_| AnalysisError::Busy)?;
        Ok(Self {
            handle,
            armed: true,
        })
    }

    /// Transfers the busy flag to the caller. The claim is disarmed; the caller is now
    /// responsible for clearing the flag (typically by constructing a `BusyGuard`).
    pub(super) fn into_arc(mut self) -> Arc<CheckerHandleInner> {
        self.armed = false;
        Arc::clone(&self.handle)
    }
}

impl Drop for BusyClaim {
    fn drop(&mut self) {
        if self.armed {
            self.handle.clear_busy();
        }
    }
}

/// RAII guard that signals a `CancellationToken` on drop unless `disarm()`-ed first.
///
/// Used to cancel the native check when the async future is dropped (e.g. by
/// `tokio::time::timeout` or `select!`) without forcing callers to thread their own token.
/// Successful completion calls `disarm()` so caller-supplied reusable tokens stay clean.
pub(super) struct CancelOnDrop {
    token: CancellationToken,
    armed: bool,
}

impl CancelOnDrop {
    pub(super) fn armed(token: CancellationToken) -> Self {
        Self { token, armed: true }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}
