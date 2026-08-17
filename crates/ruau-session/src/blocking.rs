use std::{
    any::Any,
    cell::Cell,
    error::Error as StdError,
    fmt,
    future::Future,
    panic::{self, AssertUnwindSafe},
    sync::OnceLock,
};

const DEFAULT_THREAD_NAME: &str = "ruau-session-blocking";

/// Cached runtime for blocking on non-`Send` Ruau VM futures from synchronous host APIs.
pub struct BlockingRuntime {
    runtime: OnceLock<Result<tokio::runtime::Runtime, BlockingRuntimeError>>,
    thread_name: &'static str,
}

impl BlockingRuntime {
    /// Builds a cached blocking runtime with a diagnostic thread name.
    #[must_use]
    pub const fn new(thread_name: &'static str) -> Self {
        Self {
            runtime: OnceLock::new(),
            thread_name,
        }
    }

    /// Drives `future` to completion, adapting to the caller's Tokio context.
    ///
    /// Outside Tokio, this uses the cached runtime directly. Inside a multi-thread runtime,
    /// it blocks legally through `block_in_place`. Inside a current-thread runtime or
    /// `LocalSet`, it returns [`BlockingRuntimeError::AsyncContext`].
    ///
    /// # Errors
    /// Returns [`BlockingRuntimeError`] if the cached runtime cannot be built or the caller is
    /// inside a Tokio context that cannot block.
    pub fn block_on<F>(&self, future: F) -> Result<F::Output, BlockingRuntimeError>
    where
        F: Future,
    {
        let runtime = self.runtime()?;
        let Ok(ambient) = tokio::runtime::Handle::try_current() else {
            return Ok(runtime.block_on(future));
        };
        if ambient.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
            let driven = panic::catch_unwind(AssertUnwindSafe(|| runtime.block_on(future)));
            return match driven {
                Ok(outcome) => Ok(outcome),
                Err(payload) if is_nested_runtime_panic(payload.as_ref()) => {
                    Err(BlockingRuntimeError::AsyncContext)
                }
                Err(payload) => panic::resume_unwind(payload),
            };
        }

        let entered = Cell::new(false);
        let driven = panic::catch_unwind(AssertUnwindSafe(|| {
            tokio::task::block_in_place(|| {
                entered.set(true);
                runtime.block_on(future)
            })
        }));
        match driven {
            Ok(outcome) => Ok(outcome),
            Err(payload) if entered.get() => panic::resume_unwind(payload),
            Err(_) => Err(BlockingRuntimeError::AsyncContext),
        }
    }

    fn runtime(&self) -> Result<&tokio::runtime::Runtime, BlockingRuntimeError> {
        self.runtime_with(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name(self.thread_name)
                .enable_all()
                .build()
                .map_err(|error| BlockingRuntimeError::Build(error.to_string()))
        })
    }

    fn runtime_with(
        &self,
        build: impl FnOnce() -> Result<tokio::runtime::Runtime, BlockingRuntimeError>,
    ) -> Result<&tokio::runtime::Runtime, BlockingRuntimeError> {
        match self.runtime.get_or_init(build) {
            Ok(runtime) => Ok(runtime),
            Err(error) => Err(error.clone()),
        }
    }
}

impl Default for BlockingRuntime {
    fn default() -> Self {
        Self::new(DEFAULT_THREAD_NAME)
    }
}

impl Drop for BlockingRuntime {
    fn drop(&mut self) {
        if let Some(Ok(runtime)) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Error returned by [`BlockingRuntime`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockingRuntimeError {
    /// Blocking was requested from a current-thread runtime or `LocalSet`.
    AsyncContext,
    /// The cached Tokio runtime could not be built.
    Build(String),
}

impl fmt::Display for BlockingRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AsyncContext => formatter
                .write_str("blocking Ruau execution called from a Tokio context that cannot block"),
            Self::Build(message) => {
                write!(formatter, "blocking Ruau runtime build failed: {message}")
            }
        }
    }
}

impl StdError for BlockingRuntimeError {}

fn is_nested_runtime_panic(payload: &(dyn Any + Send)) -> bool {
    panic_message(payload).is_some_and(|message| {
        message.contains("Cannot start a runtime from within a runtime")
            || message.contains("can call blocking only when running on the multi-threaded runtime")
    })
}

fn panic_message(payload: &(dyn Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
}

#[cfg(any())]
mod tests {
    use std::sync::{Arc, Barrier, atomic::AtomicUsize};

    use super::*;

    #[test]
    fn blocking_runtime_context_permits_block_in_place_bridging() {
        let runtime = BlockingRuntime::new("ruau-session-test-blocking");
        let value = runtime
            .block_on(async { tokio::task::block_in_place(|| 7) })
            .expect("blocking runtime permits block_in_place");
        assert_eq!(value, 7);
    }

    #[test]
    fn blocking_runtime_runs_inside_current_thread_spawn_blocking() {
        let runtime = std::sync::Arc::new(BlockingRuntime::new(
            "ruau-session-test-current-thread-bridge",
        ));
        let outer = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("outer runtime builds");
        let bridge = std::sync::Arc::clone(&runtime);
        let value = outer
            .block_on(async move {
                tokio::task::spawn_blocking(move || bridge.block_on(async { 11 }))
                    .await
                    .expect("spawn_blocking joins")
            })
            .expect("blocking runtime drives future");

        assert_eq!(value, 11);
    }

    #[test]
    fn concurrent_first_use_constructs_one_runtime() {
        let runtime = Arc::new(BlockingRuntime::new("ruau-session-test-first-use"));
        let barrier = Arc::new(Barrier::new(16));
        let builds = Arc::new(AtomicUsize::new(0));
        let threads = (0..16)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                let barrier = Arc::clone(&barrier);
                let builds = Arc::clone(&builds);
                std::thread::spawn(move || {
                    barrier.wait();
                    runtime
                        .runtime_with(|| {
                            builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            tokio::runtime::Builder::new_multi_thread()
                                .worker_threads(1)
                                .enable_all()
                                .build()
                                .map_err(|error| BlockingRuntimeError::Build(error.to_string()))
                        })
                        .expect("shared runtime builds")
                        .handle()
                        .clone()
                })
            })
            .collect::<Vec<_>>();

        for thread in threads {
            thread.join().expect("first-use caller completes");
        }
        assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn first_build_failure_is_cached() {
        let runtime = BlockingRuntime::new("ruau-session-test-cached-failure");
        let builds = AtomicUsize::new(0);

        for _ in 0..2 {
            let error = runtime
                .runtime_with(|| {
                    builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err(BlockingRuntimeError::Build("expected failure".to_owned()))
                })
                .expect_err("the cached build fails");
            assert_eq!(
                error,
                BlockingRuntimeError::Build("expected failure".to_owned())
            );
        }
        assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
