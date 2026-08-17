//! Prepare and run a named root with requester-relative dependencies.

use std::{num::NonZeroUsize, sync::Arc};

use ruau::{
    source::{
        InMemorySource, ModuleId, ReadContext, ReadySourceFutureExt, RootSource, SnapshotSource,
        SourceProvider,
    },
    surface::{PrepareOptions, Surface, VmConfig},
    typecheck::GraphLimits,
    vm::{SourceModuleExportPolicy, ValueSnapshot},
};

fn main() -> Result<(), String> {
    let catalog = Arc::new(InMemorySource::new().with_module(
        ModuleId::new("catalog/items/answer"),
        "return { value = 42 }",
    ));
    let root_id = ModuleId::new("catalog/startup");
    let routed = RootSource::new(
        root_id.clone(),
        "--!strict\nlocal answer = require('./items/answer')\nreturn answer.value",
    )
    .with_display_name("config/startup.luau")
    .with_root_requester(root_id.clone())
    .with_delegate(catalog);
    let sources = Arc::new(SnapshotSource::new(Arc::new(routed)));
    let entry = sources
        .resolve(None, root_id.as_bytes())
        .ready_only("resolve graph root")
        .map_err(|error| error.to_string())?;
    let root = sources
        .read_observation(ReadContext::new(&entry))
        .ready_only("read graph root")
        .map_err(|error| error.to_string())?
        .source()
        .clone();
    let surface = Surface::builder()
        .module_source(sources.clone())
        .build()
        .map_err(|error| error.to_string())?;

    let prepared = surface
        .prepare_graph_ready_with_options(
            root,
            PrepareOptions::new().with_graph_limits(GraphLimits::new(
                NonZeroUsize::new(16).expect("non-zero module limit"),
                NonZeroUsize::new(8).expect("non-zero depth limit"),
                NonZeroUsize::new(1024 * 1024).expect("non-zero byte limit"),
            )),
        )
        .map_err(|error| error.to_string())?;
    let snapshot = prepared
        .seal_sources(&sources)
        .map_err(|error| error.to_string())?;
    let mut vm = surface
        .vm_builder(
            &VmConfig::untrusted(
                ruau::vm::Ambient::deterministic(0),
                ruau::vm::Limits::metered(1_000_000, 16 * 1024 * 1024),
            )
            .with_source_module_export_policy(SourceModuleExportPolicy::DeepFrozen),
        )
        .build()
        .map_err(|error| error.to_string())?;
    let values = prepared.run(&mut vm).map_err(|error| error.to_string())?;

    assert_eq!(values, vec![ValueSnapshot::Number(42.0)]);
    println!(
        "prepared {} sealed modules at epoch {}",
        snapshot.sources().len(),
        snapshot.epoch()
    );
    Ok(())
}
