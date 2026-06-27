use std::time::{Duration, Instant};

use ruau_vm::Cancel;

use super::types::BudgetError;

/// Wall-clock deadline and cancellation token for one request.
#[derive(Clone, Debug)]
pub struct Budget {
    /// The wall-clock instant at which the request must be abandoned. The runner
    /// passes it to the VM as a host-await deadline and bridges it to
    /// cancellation so a CPU loop is stopped at this instant too.
    pub(crate) deadline: Instant,
    /// Cooperative cancellation handle. The runner observes a cancel of this
    /// signal (or the deadline) at the dispatch safepoint.
    pub(crate) cancel: Cancel,
}

impl Budget {
    /// A budget with a caller-provided wall-clock deadline and cancellation
    /// token.
    ///
    /// # Errors
    /// Returns [`BudgetError::DeadlineElapsed`] if `deadline` is already
    /// in the past or exactly now.
    pub fn new(deadline: Instant, cancel: Cancel) -> Result<Self, BudgetError> {
        if Instant::now() >= deadline {
            return Err(BudgetError::DeadlineElapsed);
        }
        Ok(Self { deadline, cancel })
    }

    /// A budget whose deadline fires `timeout` from now, with a fresh
    /// cancellation token.
    ///
    /// # Errors
    /// Returns [`BudgetError::DeadlineElapsed`] when `timeout` does not
    /// put the deadline in the future.
    pub fn with_timeout(timeout: Duration) -> Result<Self, BudgetError> {
        Self::new(Instant::now() + timeout, Cancel::manual())
    }
}
