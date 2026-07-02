//! Add one typed native module to the minimal check-and-run flow.

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

local total: number = host.add(20, 22)
return host.label, total
"#;

struct HostModule;

impl NativeModule for HostModule {
    fn name(&self) -> &str {
        "host"
    }

    fn declaration(&self) -> DeclSource<'_> {
        DeclSource::Model(host_declaration())
    }

    fn build(&self, builder: &mut dyn ModuleBuilder) {
        builder.constant(
            "label",
            ModuleBinding::library("host"),
            ModuleValue::from("calculator"),
        );
        builder.leaf_function(
            "add",
            ModuleBinding::library("host"),
            |(left, right): (f64, f64)| left + right,
        );
    }
}

fn host_declaration() -> &'static DeclModule {
    static DECLARATION: OnceLock<DeclModule> = OnceLock::new();
    DECLARATION.get_or_init(|| {
        let mut builder = Builder::new();
        builder.global(Global::new(
            "host",
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
        .module(Arc::new(HostModule))
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

    assert_eq!(json_values, serde_json::json!(["calculator", 42.0]));
    println!("{CHUNK_NAME} returned {json_values}");
    Ok(())
}
