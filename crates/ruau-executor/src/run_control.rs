use std::time::{Duration, Instant};

use ruau_vm::Cancel;

use super::types::RunControlError;

/// Wall-clock deadline and cancellation token for one request.
///
/// This is not the VM gas or memory budget; those ceilings are supplied by the
/// executor's configured [`ruau_vm::Limits`].
#[derive(Clone, Debug)]
pub struct RunControl {
    /// The wall-clock instant at which the request must be abandoned. The executor
    /// passes it to the VM as a host-await deadline and bridges it to
    /// cancellation so a CPU loop is stopped at this instant too.
    pub(crate) deadline: Instant,
    /// Cooperative cancellation handle. The executor observes a cancel of this
    /// signal (or the deadline) at the dispatch safepoint.
    pub(crate) cancel: Cancel,
}

impl RunControl {
    pub(crate) fn scoped(self) -> Self {
        let timeout = self.deadline.saturating_duration_since(Instant::now());
        Self {
            deadline: self.deadline,
            cancel: self.cancel.child_after(timeout),
        }
    }

    /// A budget with a caller-provided wall-clock deadline and cancellation
    /// token.
    ///
    /// # Errors
    /// Returns [`RunControlError::DeadlineElapsed`] if `deadline` is already
    /// in the past or exactly now.
    pub fn new(deadline: Instant, cancel: Cancel) -> Result<Self, RunControlError> {
        if Instant::now() >= deadline {
            return Err(RunControlError::DeadlineElapsed);
        }
        Ok(Self { deadline, cancel })
    }

    /// A budget whose deadline fires `timeout` from now, with a fresh
    /// cancellation token.
    ///
    /// # Errors
    /// Returns [`RunControlError::DeadlineElapsed`] when `timeout` does not
    /// put a representable deadline in the future.
    pub fn with_timeout(timeout: Duration) -> Result<Self, RunControlError> {
        let now = Instant::now();
        let deadline = now
            .checked_add(timeout)
            .filter(|deadline| *deadline > now)
            .ok_or(RunControlError::DeadlineElapsed)?;
        Ok(Self {
            deadline,
            cancel: Cancel::manual(),
        })
    }
}
