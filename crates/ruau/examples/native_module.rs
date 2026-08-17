//! Add typed native modules to the minimal check-and-run flow.
//!
//! Native modules can declare their checker surface with convenient
//! hand-authored `.d.luau` text, or with the structured `ruau-declaration` builder.
//! This example shows both styles.

use std::sync::{Arc, OnceLock};

use ruau::{
    declaration::{Builder, DeclarationSource, Field, FunctionSignature, Global, Module, Type},
    source::{ModuleId, Source},
    surface::{Surface, VmConfig},
    vm::{
        IntoLuaMulti, ModuleBinding, MultiValue, NativeModule, RuntimeError, Scope,
        borrowed_scoped_host_fn,
        module::{
            Installer as ModuleBuilder, InstallerExt as ModuleBuilderExt, Value as ModuleValue,
        },
        serde::marshaled_values_to_json_array,
    },
};

const CHUNK_NAME: &str = "native_module.luau";
const SOURCE: &str = r#"
--!strict

local parsed_total: number = parsed_host.add(20, 22)
local modeled_total: number = modeled_host.add(8, 9)
return parsed_host.label, parsed_total, parsed_host.borrowed_type({}),
    modeled_host.label, modeled_total, modeled_host.borrowed_type("text")
"#;

const PARSED_HOST_DECLARATION: &str = r#"
declare parsed_host: {
    label: string,
    add: (left: number, right: number) -> number,
    borrowed_type: (value: any) -> string,
}
"#;

struct ParsedHostModule;

impl NativeModule for ParsedHostModule {
    fn name(&self) -> &str {
        "parsed_host"
    }

    fn declaration(&self) -> DeclarationSource<'_> {
        DeclarationSource::Text(PARSED_HOST_DECLARATION)
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        build_host_module(builder, "parsed_host", "parsed declaration");
    }
}

struct ModeledHostModule;

impl NativeModule for ModeledHostModule {
    fn name(&self) -> &str {
        "modeled_host"
    }

    fn declaration(&self) -> DeclarationSource<'_> {
        DeclarationSource::Model(modeled_host_declaration())
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        build_host_module(builder, "modeled_host", "modeled declaration");
    }
}

fn build_host_module(builder: &mut dyn ModuleBuilder, module: &'static str, label: &'static str) {
    let binding = ModuleBinding::library(module);
    builder.constant("label", binding.clone(), ModuleValue::from(label));
    builder.leaf_function("add", binding.clone(), |(left, right): (f64, f64)| {
        left + right
    });
    builder.scoped_function(
        "borrowed_type",
        binding,
        borrowed_scoped_host_fn(
            |scope: &Scope<'_>, args: MultiValue<'_>| -> Result<_, RuntimeError> {
                let type_name = args.iter().next().map_or("nil", |value| value.type_name());
                type_name.into_lua_multi(scope)
            },
        ),
    );
}

fn modeled_host_declaration() -> &'static Module {
    static DECLARATION: OnceLock<Module> = OnceLock::new();
    DECLARATION.get_or_init(|| {
        let mut builder = Builder::new();
        builder.add_global(Global::new(
            "modeled_host",
            Type::table([
                Field::new("label", Type::String),
                Field::new(
                    "add",
                    Type::func(
                        FunctionSignature::new()
                            .param(("left", Type::Number))
                            .param(("right", Type::Number))
                            .ret(Type::Number),
                    ),
                ),
                Field::new(
                    "borrowed_type",
                    Type::func(
                        FunctionSignature::new()
                            .param(("value", Type::Any))
                            .ret(Type::String),
                    ),
                ),
            ]),
        ));
        builder.build().expect("host declaration validates")
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

    assert_eq!(
        json_values,
        serde_json::json!([
            "parsed declaration",
            42.0,
            "table",
            "modeled declaration",
            17.0,
            "string"
        ])
    );
    println!("{CHUNK_NAME} returned {json_values}");
    Ok(())
}
