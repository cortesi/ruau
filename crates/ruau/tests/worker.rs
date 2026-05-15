//! worker integration tests.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use ruau::{
    Error, LuauInterruptPolicy, LuauWorker, LuauWorkerCancellation, LuauWorkerError,
    LuauWorkerHandle, Result, StdLib,
};
use static_assertions::assert_impl_all;
use tokio::{
    sync::oneshot,
    time::{sleep, timeout},
};

#[cfg(test)]
mod tests {
    use super::*;

    assert_impl_all!(LuauWorkerHandle: Clone, Send, Sync);

    #[test]
    fn interrupt_policy_default_has_message() {
        let policy = LuauInterruptPolicy::default().with_deadline(Instant::now());

        let error = match policy.check() {
            Ok(_) => panic!("deadline should interrupt"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("Luau execution interrupted"));
    }

    #[test]
    fn builder_rejects_unsafe_std_libs_before_starting_worker() {
        let error = match LuauWorker::builder().std_libs(StdLib::DEBUG).build() {
            Ok(_) => panic!("debug library should require explicit unsafe VM construction"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            LuauWorkerError::UnsafeStdLibs { message } if message.contains("new_with_unchecked")
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_handle_runs_from_tokio_spawn_tasks() -> Result<()> {
        let worker = LuauWorker::builder()
            .with_setup(|lua| {
                let add = lua.create_function(|_, (a, b): (i64, i64)| Ok(a + b))?;
                lua.globals().set("add", add)
            })
            .build()
            .expect("worker");
        let handle = worker.handle();

        let mut tasks = Vec::new();
        for i in 0..8_i64 {
            let handle = handle.clone();
            tasks.push(tokio::spawn(async move {
                handle.call::<i64, _>("add", (i, i + 1)).await
            }));
        }

        let mut sum = 0;
        for task in tasks {
            sum += task.await.expect("join").expect("call");
        }
        assert_eq!(sum, 64);

        worker.shutdown().await.expect("shutdown");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_worker_future_cancels_local_task() {
        let (started_tx, started_rx) = oneshot::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let worker = LuauWorker::builder().build().expect("worker");
        let handle = worker.handle();

        let request = {
            let completed = Arc::clone(&completed);
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .with_async(move |_lua| {
                        Box::pin(async move {
                            let _ignored = started_tx.send(());
                            sleep(Duration::from_secs(30)).await;
                            completed.store(true, Ordering::SeqCst);
                            Ok::<_, ruau::Error>(())
                        })
                    })
                    .await
            })
        };

        started_rx.await.expect("request started");
        request.abort();
        drop(request.await);

        sleep(Duration::from_millis(50)).await;
        let _: () = handle
            .with(|_| Ok(()))
            .await
            .expect("worker still accepts work");
        assert!(!completed.load(Ordering::SeqCst));

        worker.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellable_worker_future_can_interrupt_busy_vm() {
        let worker = LuauWorker::builder().build().expect("worker");
        let handle = worker.handle();
        let (started_tx, started_rx) = oneshot::channel();

        let request = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .with_async_cancellable(move |lua, cancellation| {
                        Box::pin(async move {
                            let _ignored = started_tx.send(());
                            LuauInterruptPolicy::new()
                                .with_worker_cancellation(cancellation)
                                .with_message("worker request cancelled")
                                .install(lua);
                            lua.load("while true do end").exec().await
                        })
                    })
                    .await
            })
        };

        started_rx.await.expect("request started");
        request.abort();
        drop(request.await);

        timeout(
            Duration::from_secs(1),
            handle.with(|lua| {
                lua.remove_interrupt();
                Ok(())
            }),
        )
        .await
        .expect("worker accepted follow-up work")
        .expect("follow-up work succeeded");

        worker.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_policy_observes_external_cancel_flag() {
        let worker = LuauWorker::builder().build().expect("worker");
        let handle = worker.handle();
        let flag = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel();

        let request = {
            let flag = Arc::clone(&flag);
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .with_async(move |lua| {
                        Box::pin(async move {
                            let _ignored = started_tx.send(());
                            LuauInterruptPolicy::new()
                                .with_cancel_flag(flag)
                                .with_message("external cancel")
                                .install(lua);
                            lua.load("while true do end").exec().await
                        })
                    })
                    .await
            })
        };

        started_rx.await.expect("request started");
        flag.store(true, Ordering::Release);
        let result = timeout(Duration::from_secs(1), request)
            .await
            .expect("request should complete")
            .expect("join");
        assert!(matches!(result, Err(LuauWorkerError::Vm { .. })));

        worker.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_worker_error_does_not_mark_request_cancelled() {
        let worker = LuauWorker::builder().build().expect("worker");
        let handle = worker.handle();
        let (cancellation_tx, cancellation_rx) = oneshot::channel::<LuauWorkerCancellation>();

        let result = handle
            .with_async_cancellable(move |_lua, cancellation| {
                Box::pin(async move {
                    let _ignored = cancellation_tx.send(cancellation.clone());
                    Err::<(), _>(Error::runtime("ordinary failure"))
                })
            })
            .await;

        assert!(matches!(result, Err(LuauWorkerError::Vm { .. })));
        let cancellation = cancellation_rx.await.expect("cancellation token");
        assert!(!cancellation.is_cancelled());

        worker.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_shutdown_drains_accepted_requests() {
        let (started_tx, started_rx) = oneshot::channel();
        let worker = LuauWorker::builder().build().expect("worker");
        let handle = worker.handle();

        let task = tokio::spawn(async move {
            handle
                .with_async(move |_lua| {
                    Box::pin(async move {
                        let _ignored = started_tx.send(());
                        sleep(Duration::from_millis(30)).await;
                        Ok::<_, ruau::Error>(42_u32)
                    })
                })
                .await
        });

        started_rx.await.expect("request started");
        worker.shutdown().await.expect("shutdown");
        assert_eq!(task.await.expect("join").expect("request"), 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_shutdown_delivers_vm_errors_from_accepted_requests() {
        let (started_tx, started_rx) = oneshot::channel();
        let worker = LuauWorker::builder().build().expect("worker");
        let handle = worker.handle();

        let task = tokio::spawn(async move {
            handle
                .with_async(move |_lua| {
                    Box::pin(async move {
                        let _ignored = started_tx.send(());
                        sleep(Duration::from_millis(30)).await;
                        Err::<(), _>(Error::runtime("late failure"))
                    })
                })
                .await
        });

        started_rx.await.expect("request started");
        worker.shutdown().await.expect("shutdown");
        assert!(matches!(
            task.await.expect("join"),
            Err(LuauWorkerError::Vm { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retained_handle_rejects_work_after_shutdown() {
        let worker = LuauWorker::builder().build().expect("worker");
        let handle = worker.handle();

        worker.shutdown().await.expect("shutdown");

        let result = handle.with(|_| Ok(())).await;
        assert!(matches!(result, Err(LuauWorkerError::Shutdown)));
    }
}
