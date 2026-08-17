//! Register and use the optional ready-made JSON module.

use ruau::{
    module::json,
    source::{ModuleId, Source},
    surface::{Surface, VmConfig},
    vm::{Ambient, Limits},
};

fn main() -> Result<(), String> {
    let surface = Surface::builder()
        .module(json::native_module())
        .build()
        .map_err(|error| error.to_string())?;
    let prepared = surface
        .prepare(Source::text(
            ModuleId::new("json-example.luau"),
            "return json.serialize(json.object({ answer = 42 }), true)",
        ))
        .map_err(|error| error.to_string())?;
    let mut vm = surface
        .vm_builder(&VmConfig::untrusted(
            Ambient::deterministic(0),
            Limits::metered(1_000_000, 16 * 1024 * 1024),
        ))
        .build()
        .map_err(|error| error.to_string())?;
    let values = prepared.run(&mut vm).map_err(|error| error.to_string())?;
    println!("{values:?}");
    Ok(())
}
