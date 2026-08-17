//! Share the synchronized `SharedRuntime` wrapper across blocking host threads.

use std::sync::Arc;

use ruau::{
    bytecode::CompileOptions,
    session::{LoadTarget, SharedRuntime},
    source::{ModuleId, Source},
    surface::{Surface, VmConfig},
    vm::CallOptions,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let surface = Surface::new();
    let chunk = surface.compile(
        &Source::text(ModuleId::canonicalized("session"), "return 40 + 2"),
        &CompileOptions::default(),
    )?;
    let session = Arc::new(SharedRuntime::new(
        surface,
        &VmConfig::untrusted(
            ruau::vm::Ambient::deterministic(7),
            ruau::vm::Limits::unlimited(),
        ),
    )?);

    let workers = (0..2)
        .map(|worker| {
            let session = Arc::clone(&session);
            let chunk = chunk.clone();
            std::thread::spawn(move || {
                session.run_compiled_blocking(
                    &chunk,
                    &LoadTarget::named(format!("worker-{worker}.luau")),
                    CallOptions::new(),
                )
            })
        })
        .collect::<Vec<_>>();

    for worker in workers {
        let outcome = worker.join().expect("worker did not panic")?;
        println!("{:?}", outcome.values);
    }
    Ok(())
}
