//! Type-check and execute a small in-memory module graph.

use std::sync::Arc;

use ruau::{
    source::{InMemorySource, Source},
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
            .with_module("modules/mathlib", MATHLIB_SOURCE)
            .with_module("modules/message", MESSAGE_SOURCE),
    );
    let surface = Surface::builder()
        .module_source(modules)
        .build()
        .map_err(|error| format!("surface: {error}"))?;

    let source = Source::text(CHUNK_NAME, MAIN_SOURCE);
    let graph = surface.check_source_graph(&source);
    if graph.has_issues() {
        return Err(graph.diagnostics().render());
    }
    let prepared = surface.prepare(source).map_err(|error| error.to_string())?;
    let mut vm = surface
        .vm_builder(&VmConfig::metered_untrusted(0, 1_000_000, 16 * 1024 * 1024))
        .build()
        .map_err(|error| format!("build sandboxed VM: {error}"))?;
    let values = prepared
        .run_in(&mut vm)
        .map_err(|error| format!("run {CHUNK_NAME}: {error}"))?;
    let json_values = marshaled_values_to_json_array(&values).map_err(|error| error.to_string())?;

    assert_eq!(json_values, serde_json::json!(["answer = 42"]));
    println!("{CHUNK_NAME} returned {json_values}");
    Ok(())
}
