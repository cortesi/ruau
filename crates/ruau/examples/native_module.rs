//! Add typed native modules to the minimal check-and-run flow.
//!
//! Native modules can declare their checker surface with convenient
//! hand-authored `.d.luau` text, or with the structured `ruau-decl` builder.
//! This example shows both styles.

use std::sync::{Arc, OnceLock};

use ruau::{
    decl::{Builder, DeclModule, DeclSource, Field, FnSig, Global, Ty},
    source::{ModuleId, Source},
    surface::{Surface, VmConfig},
    vm::{ModuleBuilderExt, serde::marshaled_values_to_json_array},
    vm_api::{ModuleBinding, ModuleBuilder, ModuleValue, NativeModule},
};

const CHUNK_NAME: &str = "native_module.luau";
const SOURCE: &str = r#"
--!strict

local parsed_total: number = parsed_host.add(20, 22)
local modeled_total: number = modeled_host.add(8, 9)
return parsed_host.label, parsed_total, modeled_host.label, modeled_total
"#;

const PARSED_HOST_DECLARATION: &str = r#"
declare parsed_host: {
    label: string,
    add: (left: number, right: number) -> number,
}
"#;

struct ParsedHostModule;

impl NativeModule for ParsedHostModule {
    fn name(&self) -> &str {
        "parsed_host"
    }

    fn declaration(&self) -> DeclSource<'_> {
        DeclSource::Text(PARSED_HOST_DECLARATION)
    }

    fn build(&self, builder: &mut dyn ModuleBuilder) {
        build_host_module(builder, "parsed_host", "parsed declaration");
    }
}

struct ModeledHostModule;

impl NativeModule for ModeledHostModule {
    fn name(&self) -> &str {
        "modeled_host"
    }

    fn declaration(&self) -> DeclSource<'_> {
        DeclSource::Model(modeled_host_declaration())
    }

    fn build(&self, builder: &mut dyn ModuleBuilder) {
        build_host_module(builder, "modeled_host", "modeled declaration");
    }
}

fn build_host_module(builder: &mut dyn ModuleBuilder, module: &'static str, label: &'static str) {
    let binding = ModuleBinding::library(module);
    builder.constant("label", binding.clone(), ModuleValue::from(label));
    builder.leaf_function("add", binding, |(left, right): (f64, f64)| left + right);
}

fn modeled_host_declaration() -> &'static DeclModule {
    static DECLARATION: OnceLock<DeclModule> = OnceLock::new();
    DECLARATION.get_or_init(|| {
        let mut builder = Builder::new();
        builder.global(Global::new(
            "modeled_host",
            Ty::table([
                Field::new("label", Ty::String),
                Field::new(
                    "add",
                    Ty::func(
                        FnSig::new()
                            .param(("left", Ty::Number))
                            .param(("right", Ty::Number))
                            .ret(Ty::Number),
                    ),
                ),
            ]),
        ));
        builder.finish().expect("host declaration validates")
    })
}

fn main() -> Result<(), String> {
    let surface = Surface::builder()
        .module(Arc::new(ParsedHostModule))
        .module(Arc::new(ModeledHostModule))
        .build()
        .map_err(|error| format!("surface: {error}"))?;

    let source = Source::text(ModuleId::new(CHUNK_NAME), SOURCE);
    let prepared = surface.prepare(source).map_err(|error| error.to_string())?;
    let mut vm = surface
        .vm_builder(&VmConfig::metered_untrusted(0, 1_000_000, 16 * 1024 * 1024))
        .build()
        .map_err(|error| format!("build sandboxed VM: {error}"))?;
    let values = prepared
        .run_in(&mut vm)
        .map_err(|error| format!("run {CHUNK_NAME}: {error}"))?;
    let json_values = marshaled_values_to_json_array(&values).map_err(|error| error.to_string())?;

    assert_eq!(
        json_values,
        serde_json::json!(["parsed declaration", 42.0, "modeled declaration", 17.0])
    );
    println!("{CHUNK_NAME} returned {json_values}");
    Ok(())
}
