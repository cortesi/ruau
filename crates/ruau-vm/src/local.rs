//! Native local-runtime helpers for async VM entry points.

use std::{future::Future, io};

/// A reusable current-thread runtime and [`tokio::task::LocalSet`] for driving
/// async VM entry points whose futures do not need to be `Send`.
///
/// A single [`Vm`](crate::Vm) still runs one invocation at a time; this helper
/// only supplies the executor shape for `exec_async`/async host functions. Use
/// [`run_local`] for one-off calls and keep a `LocalExecutor` when running many
/// scripts from the same host thread.
pub struct LocalExecutor {
    runtime: tokio::runtime::Runtime,
    local: tokio::task::LocalSet,
}

impl LocalExecutor {
    /// Builds a current-thread Tokio runtime with timers and IO enabled.
    ///
    /// # Errors
    /// Returns Tokio's runtime-construction error if the host cannot allocate
    /// the native executor resources.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            local: tokio::task::LocalSet::new(),
        })
    }

    /// Runs `future` to completion on this executor's local task set.
    ///
    /// The future may borrow a [`Vm`](crate::Vm), may be `!Send`, and may use
    /// `tokio::task::spawn_local` internally. To bound a parked async host
    /// await, pass a wall-clock [`Deadline`](crate::Deadline) or
    /// [`CancellationToken`](crate::CancellationToken) through the call's
    /// [`Limits`](crate::Limits).
    pub fn run<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        self.local.block_on(&self.runtime, future)
    }
}

/// Builds a temporary [`LocalExecutor`] and runs `future` to completion.
///
/// # Errors
/// Returns Tokio's runtime-construction error if the host cannot allocate the
/// native executor resources.
pub fn run_local<F>(future: F) -> io::Result<F::Output>
where
    F: Future,
{
    Ok(LocalExecutor::new()?.run(future))
}
