//! Sharing a dedicated Luau worker from a multi-thread Tokio runtime.

use ruau::{LuauWorker, Result};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let worker = LuauWorker::builder()
        .with_setup(|lua| {
            let greet = lua.create_function(|_, name: String| Ok(format!("hello, {name}")))?;
            lua.globals().set("greet", greet)
        })
        .build()
        .expect("worker");

    let handle = worker.handle();
    let mut tasks = Vec::new();
    for name in ["Ada", "Grace", "Barbara"] {
        let handle = handle.clone();
        tasks.push(tokio::spawn(async move {
            handle.call::<String, _>("greet", name.to_owned()).await
        }));
    }

    for task in tasks {
        println!("{}", task.await.expect("task").expect("worker call"));
    }

    worker.shutdown().await.expect("shutdown");
    Ok(())
}
