//! The engine's cancellation signal.

use std::{
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Duration,
};

/// The foreign cancellation primitive [`Cancel::new`] consumes, re-exported so
/// embedders can construct and link tokens without adding a direct
/// `tokio-util` dependency (and without risking a version mismatch with the
/// one the engine was built against).
pub use tokio_util::sync::CancellationToken;

/// A cancellation signal the engine reads at its safepoints. It is a thin wrapper
/// over the cancellation primitive so the **synchronous engine core depends on
/// `Cancel`, not on Tokio directly**: the safepoint check is
/// `Cancel::is_cancelled`.
///
/// `Option<Cancel>` is the "configured or not" state: `None` is an uncancellable
/// run, and a `Limits` overlay inherits an unset `cancel` from its base.
#[derive(Clone, Debug)]
pub struct Cancel {
    token: CancellationToken,
    watchdog_release: Option<Arc<WatchdogRelease>>,
}

#[derive(Debug)]
struct WatchdogRelease {
    state: Arc<WatchdogState>,
}

#[derive(Debug)]
struct WatchdogState {
    released: Mutex<bool>,
    condvar: Condvar,
}

impl WatchdogState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            released: Mutex::new(false),
            condvar: Condvar::new(),
        })
    }

    fn release(&self) {
        if let Ok(mut released) = self.released.lock() {
            *released = true;
            self.condvar.notify_one();
        }
    }

    fn wait_until_released_or_timeout(&self, timeout: Duration) -> bool {
        let Ok(released) = self.released.lock() else {
            return true;
        };
        let Ok((released, _)) = self
            .condvar
            .wait_timeout_while(released, timeout, |released| !*released)
        else {
            return true;
        };
        *released
    }
}

impl Drop for WatchdogRelease {
    fn drop(&mut self) {
        self.state.release();
    }
}

impl Cancel {
    /// Wraps a cancellation token as the engine's cancellation signal.
    #[must_use]
    pub fn new(token: CancellationToken) -> Self {
        Self {
            token,
            watchdog_release: None,
        }
    }

    /// A fresh, manually-triggered signal: call [`Cancel::cancel`] to fire it.
    /// `CancellationToken` is runtime-free, so this works in fully synchronous
    /// embeddings with no Tokio runtime.
    #[must_use]
    pub fn manual() -> Self {
        Self::new(CancellationToken::new())
    }

    /// Requests cancellation; every clone of this signal observes it at the
    /// next dispatch safepoint.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// A wall-clock watchdog for synchronous VM calls:
    /// the returned signal fires after `timeout` on a detached watchdog
    /// thread. The synchronous engine otherwise enforces only gas and logical
    /// deadlines — without this (or an externally cancelled signal), a
    /// `Deadline::Wall` is only honored by the async driver's governed await.
    ///
    #[must_use]
    pub fn after(timeout: Duration) -> Self {
        let token = CancellationToken::new();
        let state = WatchdogState::new();
        let thread_state = Arc::clone(&state);
        let armed = token.clone();
        thread::Builder::new()
            .name("ruau-cancel-watchdog".to_owned())
            .spawn(move || {
                if !thread_state.wait_until_released_or_timeout(timeout) {
                    armed.cancel();
                }
            })
            .ok();
        Self {
            token,
            watchdog_release: Some(Arc::new(WatchdogRelease { state })),
        }
    }

    /// Whether cancellation has been requested. The synchronous safepoint check.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Resolves when cancellation is requested. Used by the async driver's
    /// governed await and the runner's stage selects.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// A child signal: cancelling `self` cancels the child, while cancelling
    /// the child leaves `self` untouched. The runner scopes each execution
    /// stage with one.
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            token: self.token.child_token(),
            watchdog_release: self.watchdog_release.clone(),
        }
    }
}

#[cfg(any())]
mod tests {
    use std::sync::Arc;

    use tokio_util::sync::CancellationToken;

    use super::{Cancel, WatchdogRelease, WatchdogState};

    #[test]
    fn is_cancelled_reflects_the_backing_token() {
        let token = CancellationToken::new();
        let cancel = Cancel::new(token.clone());
        assert!(!cancel.is_cancelled());
        token.cancel();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn dropping_the_last_watchdog_signal_releases_the_watchdog() {
        let state = WatchdogState::new();
        let release = WatchdogRelease {
            state: state.clone(),
        };
        let clone = Arc::new(release);
        let other = Arc::clone(&clone);
        drop(clone);
        assert!(!*state.released.lock().expect("watchdog state lock"));
        drop(other);
        assert!(*state.released.lock().expect("watchdog state lock"));
    }
}
