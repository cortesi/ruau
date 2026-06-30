//! Minimal embedding: type-check, compile, and execute one script.

use std::error::Error;

use ruau::{
    source::Source,
    surface::{Surface, VmConfig},
    vm::serde::marshaled_values_to_json_array,
};

const CHUNK_NAME: &str = "check_and_run.luau";
const SOURCE: &str = r#"
--!strict

local function square(value: number): number
    return value * value
end

return square(6), "checked"
"#;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let surface = Surface::new();
    let source = Source::text(CHUNK_NAME, SOURCE);
    let prepared = surface.prepare(source)?;
    let mut vm = surface
        .vm_builder(&VmConfig::metered_untrusted(0, 1_000_000, 16 * 1024 * 1024))
        .build()?;
    let values = prepared.run_in(&mut vm)?;
    let json_values = marshaled_values_to_json_array(&values)?;

    assert_eq!(json_values, serde_json::json!([36.0, "checked"]));
    println!("{CHUNK_NAME} returned {json_values}");
    Ok(())
}
