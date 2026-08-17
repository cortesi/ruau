//! Type-check and execute a small in-memory module graph.

use std::sync::Arc;

use ruau::{
    source::{InMemorySource, ModuleId, Source},
    surface::{Surface, VmConfig},
    vm::serde::marshaled_values_to_json_array,
};

const CHUNK_NAME: &str = "modules/main.luau";
const MAIN_SOURCE: &str = r#"
--!strict

local mathlib = require("modules/mathlib")
local message = require("modules/message")

return message.format(mathlib.double(21))
"#;

const MATHLIB_SOURCE: &str = r#"
--!strict

local mathlib = {}

function mathlib.double(value: number): number
    return value * 2
end

return mathlib
"#;

const MESSAGE_SOURCE: &str = r#"
--!strict

local message = {}

function message.format(value: number): string
    return "answer = " .. tostring(value)
end

return message
"#;

fn main() -> Result<(), String> {
    let modules = Arc::new(
        InMemorySource::new()
            .with_module(ModuleId::new("modules/mathlib"), MATHLIB_SOURCE)
            .with_module(ModuleId::new("modules/message"), MESSAGE_SOURCE),
    );
    let surface = Surface::builder()
        .module_source(modules)
        .build()
        .map_err(|error| format!("surface: {error}"))?;

    let source = Source::text(ModuleId::new(CHUNK_NAME), MAIN_SOURCE);
    let graph = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::overlay(&source),
            Default::default(),
        )
        .map_err(|error| error.to_string())?;
    if graph.has_issues() {
        return Err(graph.diagnostics().render());
    }
    let prepared = surface.prepare(source).map_err(|error| error.to_string())?;
    let mut vm = surface
        .vm_builder(&VmConfig::untrusted(
            ruau::vm::Ambient::deterministic(0),
            ruau::vm::Limits::metered(1_000_000, 16 * 1024 * 1024),
        ))
        .build()
        .map_err(|error| format!("build sandboxed VM: {error}"))?;
    let values = prepared
        .run(&mut vm)
        .map_err(|error| format!("run {CHUNK_NAME}: {error}"))?;
    let json_values = marshaled_values_to_json_array(&values).map_err(|error| error.to_string())?;

    assert_eq!(json_values, serde_json::json!(["answer = 42"]));
    println!("{CHUNK_NAME} returned {json_values}");
    Ok(())
}
