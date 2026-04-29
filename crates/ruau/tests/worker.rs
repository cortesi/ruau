#![allow(
    missing_docs,
    clippy::absolute_paths,
    clippy::items_after_statements,
    clippy::missing_docs_in_private_items,
    clippy::tests_outside_test_module
)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use ruau::{LuauWorker, LuauWorkerHandle, Result};
use static_assertions::assert_impl_all;

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
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        completed.store(true, Ordering::SeqCst);
                        Ok::<_, ruau::Error>(())
                    })
                })
                .await
        })
    };

    while started.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    request.abort();
    let _ = request.await;

    tokio::time::sleep(Duration::from_millis(50)).await;
    let _: () = handle.with(|_| Ok(())).await.expect("worker still accepts work");
    assert!(!completed.load(Ordering::SeqCst));

    worker.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_shutdown_drains_accepted_requests() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let worker = LuauWorker::builder().build().expect("worker");
    let handle = worker.handle();

    let task = tokio::spawn(async move {
        handle
            .with_async(move |_lua| {
                Box::pin(async move {
                    let _ = started_tx.send(());
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok::<_, ruau::Error>(42_u32)
                })
            })
            .await
    });

    started_rx.await.expect("request started");
    worker.shutdown().await.expect("shutdown");
    assert_eq!(task.await.expect("join").expect("request"), 42);
}
