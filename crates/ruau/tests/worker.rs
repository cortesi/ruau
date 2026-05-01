//! worker integration tests.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use ruau::{Error, LuauWorker, LuauWorkerHandle, Result, VmState};
use static_assertions::assert_impl_all;
use tokio::{
    sync::oneshot,
    time::{sleep, timeout},
};

#[cfg(test)]
mod tests {
    use super::*;

    assert_impl_all!(LuauWorkerHandle: Clone, Send, Sync);

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
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicBool::new(false));
        let worker = LuauWorker::builder().build().expect("worker");
        let handle = worker.handle();

        let request = {
            let started = Arc::clone(&started);
            let completed = Arc::clone(&completed);
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .with_async(move |_lua| {
                        Box::pin(async move {
                            started.fetch_add(1, Ordering::SeqCst);
                            sleep(Duration::from_secs(30)).await;
                            completed.store(true, Ordering::SeqCst);
                            Ok::<_, ruau::Error>(())
                        })
                    })
                    .await
            })
        };

        while started.load(Ordering::SeqCst) == 0 {
            sleep(Duration::from_millis(5)).await;
        }
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
                            lua.set_interrupt(move |_| match cancellation.is_cancelled() {
                                true => Err(Error::runtime("worker request cancelled")),
                                false => Ok(VmState::Continue),
                            });
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
}
