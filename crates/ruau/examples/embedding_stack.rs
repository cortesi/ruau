//! Compose mounts, graph preparation, generated modules, retained execution,
//! and per-call context without application-specific glue.

use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

use ruau::{
    declaration::{FunctionSignature, Type},
    module::{Binding, Builder},
    session::Runtime,
    source::fs::DirectoryMounts,
    surface::{PrepareOptions, Surface, VmConfig},
    vm::{CallOptions, Limits, Scope, ValueSnapshot},
};

struct Multiplier(f64);

fn write(path: &Path, source: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, source)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::temp_dir().join(format!("ruau-embedding-stack-{}", std::process::id()));
    let app = base.join("app");
    let shared = base.join("shared");
    write(
        &app.join("main.luau"),
        "local dep = require('./dep')\nlocal base = require('@shared/base')\nprint('running')\nreturn host.scale(dep + base)",
    )?;
    write(&app.join("dep.luau"), "return 20")?;
    write(&shared.join("base.luau"), "return 1")?;

    let mounts = DirectoryMounts::builder()
        .mount("@app", &app)
        .mount("@shared", &shared)
        .build()?;
    let mut native = Builder::new("host");
    native.scoped_function_fn(
        "scale",
        Binding::library(
            "host",
            Type::func(
                FunctionSignature::new()
                    .param(("value", Type::Number))
                    .ret(Type::Number),
            ),
        ),
        |scope: &Scope<'_>, (value,): (f64,)| {
            let multiplier = scope
                .app_data::<Multiplier>()
                .map_or(1.0, |multiplier| multiplier.0);
            Ok(value * multiplier)
        },
    );
    let surface = Surface::builder()
        .module(native.build()?)
        .module_source(Arc::new(mounts.clone()))
        .build()?;
    let mut runtime = Runtime::new(
        surface,
        &VmConfig::untrusted(
            ruau::vm::Ambient::deterministic(0),
            ruau::vm::Limits::unlimited(),
        ),
    )?;
    let source = mounts.source_for_path(app.join("main.luau"))?;
    let prepared = runtime.prepare_ready(source.source().clone(), PrepareOptions::new())?;
    let root = runtime.load_prepared(&prepared)?;
    let prints = Arc::new(Mutex::new(Vec::<u8>::new()));
    let print_capture = Arc::clone(&prints);
    let values = runtime.run_ready(
        &root,
        CallOptions::new()
            .limits(Limits::unlimited())
            .app_data(Multiplier(2.0))
            .print_sink(Box::new(move |line| {
                print_capture
                    .lock()
                    .expect("print lock")
                    .extend_from_slice(line);
            })),
    )?;
    assert_eq!(values, vec![ValueSnapshot::Number(42.0)]);
    assert_eq!(*prints.lock().expect("prints"), b"running\n".to_vec());
    runtime.unload(&root)?;
    fs::remove_dir_all(base)?;
    Ok(())
}
