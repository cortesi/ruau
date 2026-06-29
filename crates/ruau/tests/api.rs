//! Public API smoke tests for downstream crate use.
#![allow(clippy::tests_outside_test_module)]

use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    process::{Command, id},
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ruau::{
    analysis::resolve::config::EmptyResolver,
    ast::{
        parse::{Options, SyntaxFlags, parse_file, parse_file_with},
        syntax::{Stat, Type},
    },
    fs::FilesystemSource,
    source::{InMemorySource, ModuleName, SourceMetadata},
    typecheck::frontend::GraphChecker,
    vm::{HostTypeBuilder, ModuleBuilderExt},
};

fn block_on_test<F: Future>(future: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => {}
        }
    }
}

fn temp_root(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ruau-api-{name}-{}-{nanos}", id()));
    remove_dir(&root);
    fs::create_dir_all(&root).expect("temporary root can be created");
    root
}

fn write_file(path: &Path, contents: &str) {
    write_bytes(path, contents.as_bytes());
}

fn write_bytes(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory can be created");
    }
    fs::write(path, contents).expect("file can be written");
}

fn remove_dir(path: &Path) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", path.display()),
    }
}

fn remove_file(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", path.display()),
    }
}

fn libraries_except(library: ruau::vm::Library) -> impl Iterator<Item = ruau::vm::Library> {
    ruau::vm::Library::ALL
        .iter()
        .copied()
        .filter(move |candidate| *candidate != library)
}

#[test]
fn public_facade_exposes_common_embedder_entrypoints() {
    let runtime_capabilities =
        ruau::vm::RuntimeCapabilities::from_libraries([ruau::vm::Library::String]);
    let compile_options = ruau::bytecode::CompileOptions::default();
    let _chunk = runtime_capabilities
        .compile_source(b"return string.len('hello')", &compile_options)
        .expect("facade runtime-capability compile path works");

    let _vm = ruau::vm::Vm::builder()
        .runtime_capabilities(runtime_capabilities)
        .ambient(ruau::vm::Ambient::deterministic(0))
        .limits(ruau::vm::Limits::unlimited())
        .build()
        .expect("facade VM builder path works");
}

#[test]
fn umbrella_only_derive_fixture_runs_outside_ruau_graph() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = manifest_dir.join("tests/fixtures/derive_umbrella/Cargo.toml");
    let nested_target = manifest_dir.join("tests/fixtures/derive_umbrella/target");
    let target_dir = temp_root("derive-umbrella-target");
    remove_dir(&nested_target);

    let output = Command::new(env!("CARGO"))
        .arg("run")
        .arg("--quiet")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target_dir)
        .env("CARGO_INCREMENTAL", "0")
        .output()
        .expect("cargo run can run for derive umbrella fixture");

    remove_dir(&target_dir);
    assert!(
        output.status.success(),
        "derive umbrella fixture failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !nested_target.exists(),
        "fixture check must not leave a nested target directory"
    );
}

fn filesystem_source_runner(root: &Path) -> ruau::runner::Runner {
    let source = std::sync::Arc::new(FilesystemSource::new(root));
    let surface = ruau::surface::Surface::builder()
        .module_source(source)
        .build()
        .expect("filesystem-backed surface validates");

    ruau::runner::Runner::builder()
        .surface(surface)
        .ambient(ruau::vm::Ambient::production(0))
        .limits(ruau::vm::Limits {
            gas: Some(100_000),
            max_memory_bytes: Some(1 << 20),
            ..ruau::vm::Limits::unlimited()
        })
        .lane_count(1)
        .lane_admission_limits(ruau::runner::AdmissionLimits {
            max_in_flight: 1,
            max_in_flight_per_tenant: 1,
            max_queued: 1,
            max_queued_per_tenant: 1,
            max_total: 2,
        })
        .features(ruau::vm::ExecutionFeatures::all_off())
        .max_source_bytes(1024)
        .build()
        .expect("runner validates")
}

#[test]
fn downstream_retained_session_builder_path_is_live() {
    let _builder: ruau::vm::VmBuilder = ruau::vm::Vm::builder();
}

#[test]
fn downstream_users_can_parse_declaration_syntax_through_umbrella() {
    let parsed = parse_file_with(
        "declare module: { ping: (message: string?) -> string }\n",
        Options {
            allow_declaration_syntax: true,
            ..Options::default()
        },
        SyntaxFlags::all_luau(),
    );

    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    let Some(Stat::Block { body, .. }) = parsed.root else {
        panic!("expected parsed root block");
    };
    let Stat::DeclareGlobal { luau_type, .. } = &body[0] else {
        panic!("expected declaration global");
    };
    assert!(matches!(luau_type.as_ref(), Type::Table { .. }));
}

#[test]
fn downstream_users_can_parse() {
    let result = parse_file("return require(script.Module)");
    assert!(result.errors.is_empty());
    assert!(result.root.is_some(), "parser produces a root");
}

#[test]
fn downstream_users_can_trace_and_parse_module_graphs() {
    let sources = InMemorySource::new()
        .with_module("main", r#"return require("dep")"#)
        .with_module("dep", "return {}");
    let config = EmptyResolver;
    let mut checked = GraphChecker::new(&sources, &config);

    let graph = block_on_test(checked.check_async("main"));
    let trace = checked
        .frontend()
        .require_trace(&ModuleName::from("main"))
        .expect("main trace exists");

    assert_eq!(
        trace
            .require_list
            .iter()
            .map(|entry| entry.module.as_str())
            .collect::<Vec<_>>(),
        ["dep"]
    );
    assert_eq!(
        graph
            .build_queue
            .iter()
            .map(ModuleName::as_str)
            .collect::<Vec<_>>(),
        ["dep", "main"]
    );
}

#[test]
fn downstream_users_can_extract_module() {
    let sources = InMemorySource::new()
        .with_module(
            "dep",
            "--!strict\nexport type DepRow = { name: string }\nreturn 3",
        )
        .with_module(
            "main",
            "--!strict\n\
             local dep = require(\"dep\")\n\
             export type Handler = (number) -> string\n\
             export type Row = { id: number }\n\
             return function(value: number): string return tostring(value + dep) end",
        );
    let config = EmptyResolver;
    let mut frontend = ruau::typecheck::frontend::GraphChecker::new(&sources, &config);

    block_on_test(frontend.check_async("main"));
    let checked = frontend
        .checked_module(&ModuleName::from("main"))
        .expect("main checked");
    let schema = ruau::typecheck::schema::extract_module(frontend.checker().arena(), checked);

    assert!(!schema.has_errors(), "{:?}", schema.diagnostics);
    assert_eq!(schema.exported_functions().count(), 1);
    assert_eq!(schema.exported_tables().count(), 1);
    assert!(
        schema
            .imported_modules
            .contains_key(&ModuleName::from("dep"))
    );
    assert!(
        schema.return_types[0]
            .summary
            .as_ref()
            .is_some_and(|summary| { summary.contains("(number)") && summary.contains("string") })
    );
}

#[test]
fn downstream_users_can_extract_source_aware_schema_diagnostics() {
    let sources = InMemorySource::new()
        .with_module(
            "dep",
            "--!strict\nlocal value: number = \"bad\"\nreturn value",
        )
        .with_metadata("dep", SourceMetadata::new("tenant/dep.luau"))
        .with_module("main", "--!strict\nreturn require(\"dep\")");
    let config = EmptyResolver;
    let mut frontend = ruau::typecheck::frontend::GraphChecker::new(&sources, &config);

    block_on_test(frontend.check_async("main"));
    let schema = ruau::typecheck::schema::extract_frontend(&frontend, &ModuleName::from("main"))
        .expect("main checked");

    let diagnostic = schema
        .source_diagnostics
        .iter()
        .find(|entry| entry.module == ModuleName::from("dep"))
        .expect("dep diagnostic is reported");
    assert!(schema.has_errors());
    assert_eq!(diagnostic.display_name, "tenant/dep.luau");
    assert_ne!(
        diagnostic.diagnostic.category,
        ruau::typecheck::diagnostics::DiagnosticCategory::Resolver
    );
}

#[test]
fn downstream_users_can_name_curated_embedding_surface() {
    fn add_one(_: &ruau::vm::Scope<'_>, value: i64) -> Result<i64, ruau::vm::RuntimeError> {
        Ok(value + 1)
    }

    fn assert_string_conversion<'s, T>()
    where
        T: ruau::vm::IntoLua<'s> + ruau::vm::FromLua<'s>,
    {
    }

    let _host: Box<dyn ruau::vm::ScopedHostFunction> = ruau::vm::scoped_host_fn(add_one);
    let _async_host: Box<dyn ruau::vm::AsyncHostFunction> =
        ruau::vm::async_host_fn(|ctx: ruau::vm::AsyncHostContext, value: i64| async move {
            let value = ctx.scope(move |_| Ok(value + 1)).await?;
            Ok(ruau::vm_api::HostReturn {
                values: vec![ruau::vm_api::OwnedValue::Integer(value)],
            })
        });
    assert_string_conversion::<String>();
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_async_hosts_return_stashed_tables() {
    struct VerberModule;

    impl ruau::vm_api::NativeModule for VerberModule {
        fn name(&self) -> &str {
            "verber"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text({
                "declare verber: { make: () -> { answer: number, label: string } }"
            })
        }

        fn build(&self, builder: &mut dyn ruau::vm_api::ModuleBuilder) {
            use ruau::vm::{IntoHostReturn, ModuleBuilderExt};

            builder.async_function(
                "make",
                ruau::vm_api::ModuleBinding::library("verber"),
                ruau::vm::async_host_fn(|ctx: ruau::vm::AsyncHostContext, (): ()| async move {
                    let table = ctx
                        .scope(|scope| {
                            let table = scope.create_table()?;
                            table.set(scope, "answer", 42.0)?;
                            table.set(scope, "label", "built")?;
                            scope.stash_table(table)
                        })
                        .await?;
                    Ok(ruau::vm_api::HostReturn {
                        values: table.into_host_return()?,
                    })
                }),
            );
        }
    }

    let surface = ruau::surface::Surface::builder()
        .libraries([])
        .module(std::sync::Arc::new(VerberModule))
        .build()
        .expect("surface validates");
    let mut vm = surface
        .vm_builder(
            ruau::vm::Ambient::production(0),
            ruau::vm::Limits {
                gas: Some(100_000),
                max_memory_bytes: Some(1 << 20),
                ..ruau::vm::Limits::unlimited()
            },
        )
        .build_sandboxed()
        .expect("vm builds");
    let chunk = surface
        .compile(
            b"local t = verber.make()\n\
              assert(t.answer == 42, \"wrong answer\")\n\
              assert(t.label == \"built\", \"wrong label\")\n\
              return 0",
            &ruau::bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
    let loaded = vm.load(&chunk).expect("load");
    if let Err(error) = vm.call_async(&loaded, Default::default()).await {
        let detail = match error.error {
            ruau_vm_api::RawValue::String(handle) => {
                let bytes = vm.heap().string(handle).expect("error string").bytes();
                String::from_utf8_lossy(bytes).into_owned()
            }
            other => format!("{other:?}"),
        };
        panic!("stashed table returns through async host: {detail}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_users_can_run_with_curated_runner_surface() {
    use std::{sync::Arc, time::Duration};

    let source = Arc::new(ruau::source::InMemorySource::new().with_module("dep", "return 37"));
    let surface = ruau::surface::Surface::builder()
        .libraries([])
        .module_source(source)
        .build()
        .expect("surface validates");

    let runner = ruau::runner::Runner::builder()
        .surface(surface)
        .ambient(ruau::vm::Ambient::production(0))
        .limits(ruau::vm::Limits {
            gas: Some(100_000),
            max_memory_bytes: Some(1 << 20),
            ..ruau::vm::Limits::unlimited()
        })
        .lane_count(2)
        .lane_admission_limits(ruau::runner::AdmissionLimits {
            max_in_flight: 2,
            max_in_flight_per_tenant: 1,
            max_queued: 2,
            max_queued_per_tenant: 1,
            max_total: 4,
        })
        .features(ruau::vm::ExecutionFeatures::all_off())
        .max_source_bytes(1024)
        .build()
        .expect("runner validates");
    assert_eq!(runner.lane_count(), 2);
    assert_eq!(runner.lane_metrics().lanes, 2);

    let outcome = runner
        .run(ruau::runner::Request::new(
            br#"return require("dep") + 5"#,
            ruau::runner::Budget::with_timeout(Duration::from_secs(5)).expect("future deadline"),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(
        outcome.values.as_slice(),
        &[ruau::runner::ResultValue::Number(42.0)]
    );
    assert!(runner.report_metadata().module_source_granted);
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_users_can_run_multi_tenant_runner_paths() {
    let surface = ruau::surface::Surface::builder()
        .build()
        .expect("surface validates");
    let runner = ruau::runner::Runner::builder()
        .surface(surface)
        .ambient(ruau::vm::Ambient::production(0))
        .limits(ruau::vm::Limits {
            gas: Some(100_000),
            max_memory_bytes: Some(1 << 20),
            ..ruau::vm::Limits::unlimited()
        })
        .lane_count(2)
        .lane_admission_limits(ruau::runner::AdmissionLimits {
            max_in_flight: 2,
            max_in_flight_per_tenant: 1,
            max_queued: 2,
            max_queued_per_tenant: 1,
            max_total: 4,
        })
        .features(ruau::vm::ExecutionFeatures::all_off())
        .max_source_bytes(1024)
        .build()
        .expect("runner validates");

    let alpha = ruau::runner::TenantId(1);
    let beta = ruau::runner::TenantId(2);
    let alpha_source = b"return 11";
    let beta_source = b"return 22";

    let alpha_report = runner
        .run_report(
            ruau::runner::Request::new(
                alpha_source,
                ruau::runner::Budget::with_timeout(std::time::Duration::from_secs(5))
                    .expect("future deadline"),
            )
            .tenant(alpha),
        )
        .await;
    let beta_report = runner
        .run_report(
            ruau::runner::Request::new(
                beta_source,
                ruau::runner::Budget::with_timeout(std::time::Duration::from_secs(5))
                    .expect("future deadline"),
            )
            .tenant(beta),
        )
        .await;

    assert_eq!(alpha_report.tenant, alpha);
    assert_eq!(beta_report.tenant, beta);
    match alpha_report.outcome {
        ruau::runner::RequestReportOutcome::Success { values } => {
            assert_eq!(values, vec![ruau::runner::ResultValue::Number(11.0)]);
        }
        other => panic!("alpha tenant should succeed, got {other:?}"),
    }
    match beta_report.outcome {
        ruau::runner::RequestReportOutcome::Success { values } => {
            assert_eq!(values, vec![ruau::runner::ResultValue::Number(22.0)]);
        }
        other => panic!("beta tenant should succeed, got {other:?}"),
    }

    let alpha_totals = runner.tenant_resource_totals(alpha);
    let beta_totals = runner.tenant_resource_totals(beta);
    assert_eq!(alpha_totals.requests, 1);
    assert_eq!(
        alpha_totals.source_bytes,
        u64::try_from(alpha_source.len()).expect("source length fits")
    );
    assert_eq!(beta_totals.requests, 1);
    assert_eq!(
        beta_totals.source_bytes,
        u64::try_from(beta_source.len()).expect("source length fits")
    );
    assert_eq!(runner.lane_metrics().lanes, 2);
}

#[test]
fn downstream_users_can_reuse_surface_checker_for_schema_checks() {
    let sources = InMemorySource::new()
        .with_module(
            "dep",
            "--!strict\nexport type Dep = { value: number }\nreturn 3",
        )
        .with_module(
            "main",
            "--!strict\n\
             local dep = require(\"dep\")\n\
             export type Handler = (number) -> string\n\
             return function(value: number): string return tostring(value + dep) end",
        );
    let surface = ruau::surface::Surface::builder()
        .module_source(std::sync::Arc::new(sources.clone()))
        .build()
        .expect("surface validates");
    let config = EmptyResolver;
    let mut frontend = ruau::typecheck::frontend::GraphChecker::with_checker(
        &sources,
        &config,
        surface.new_checker(),
    );

    block_on_test(frontend.check_async("main"));
    let schema = ruau::typecheck::schema::extract_frontend(&frontend, &ModuleName::from("main"))
        .expect("main checked");

    assert!(!schema.has_errors(), "{:?}", schema.source_diagnostics);
    assert_eq!(schema.exported_functions().count(), 1);
    assert!(
        schema
            .imported_modules
            .contains_key(&ModuleName::from("dep"))
    );
}

#[test]
fn downstream_surface_checker_without_module_source_rejects_require() {
    let surface = ruau::surface::Surface::builder()
        .build()
        .expect("sourceless surface validates");
    let mut checker = surface.new_checker();

    let checked = checker.check_source_bytes_with_config(
        br#"--!strict
return require("dep")
"#,
        ruau::typecheck::checker::Config::with_source_mode(
            ruau::analysis::resolve::AnalysisMode::Strict,
        ),
    );
    let summary = checked.diagnostics().render("sourceless.luau");

    assert!(checked.has_errors(), "{summary}");
    assert!(
        checked
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.category
                == ruau::typecheck::diagnostics::DiagnosticCategory::UnknownSymbol),
        "{summary}"
    );
}

#[test]
fn downstream_surface_checks_source_bytes_with_surface_mode() {
    let surface = ruau::surface::Surface::builder()
        .analysis_mode(ruau::analysis::resolve::AnalysisMode::Nonstrict)
        .build()
        .expect("surface validates");

    let checked = surface
        .check_source_bytes(b"local x: { foo: string }? = nil\nlocal y = x.foo\nlocal _ = y");

    assert_eq!(
        checked.mode(),
        ruau::analysis::resolve::AnalysisMode::Nonstrict
    );
    assert!(
        !checked.has_errors(),
        "{}",
        checked.diagnostics().render("nonstrict.luau")
    );
}

#[test]
fn downstream_surface_check_config_override_wins_over_surface_mode() {
    let surface = ruau::surface::Surface::builder()
        .analysis_mode(ruau::analysis::resolve::AnalysisMode::Nonstrict)
        .build()
        .expect("surface validates");

    let checked = surface.check_source_bytes_with_config(
        b"local x: { foo: string }? = nil\nlocal y = x.foo\nlocal _ = y",
        ruau::typecheck::checker::Config::with_source_mode(
            ruau::analysis::resolve::AnalysisMode::Strict,
        ),
    );

    assert_eq!(
        checked.mode(),
        ruau::analysis::resolve::AnalysisMode::Strict
    );
    assert!(
        checked.has_errors(),
        "strict override should reject nil property read"
    );
}

#[test]
fn downstream_checker_extracts_schema_without_naming_arena() {
    let mut checker = ruau::typecheck::checker::Checker::new();
    let checked = checker.check_source(
        "--!strict\n\
         export type Handler = (number) -> string\n\
         return function(value: number): string return tostring(value) end",
    );

    let schema = checker.extract_schema(&checked);

    assert!(!schema.has_errors(), "{:?}", schema.diagnostics);
    assert_eq!(schema.exported_functions().count(), 1);
    assert_eq!(schema.return_types.len(), 1);
}

#[test]
fn downstream_frontend_surface_checker_types_native_require_exports() {
    struct NativeRequireModule;

    impl ruau::vm_api::NativeModule for NativeRequireModule {
        fn name(&self) -> &str {
            "native"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare native: { answer: () -> number }")
        }

        fn export(&self) -> ruau::vm_api::ModuleExport {
            ruau::vm_api::ModuleExport::Require
        }

        fn build(&self, builder: &mut dyn ruau::vm_api::ModuleBuilder) {
            builder.leaf_function(
                "answer",
                ruau::vm_api::ModuleBinding::library("native"),
                |(): ()| 42.0_f64,
            );
        }
    }

    let sources = InMemorySource::new().with_module(
        "main",
        "--!strict\nlocal native = require(\"native\")\nlocal answer: number = native.answer()\nreturn answer\n",
    );
    let surface = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .module(std::sync::Arc::new(NativeRequireModule))
        .build()
        .expect("native require surface validates");
    let config = EmptyResolver;
    let mut frontend = GraphChecker::with_checker(&sources, &config, surface.new_checker());

    let graph = block_on_test(frontend.check_async("main"));
    let diagnostics = frontend.graph_diagnostics(&graph);

    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(
        !frontend
            .checked_module(&ModuleName::from("main"))
            .expect("main checked")
            .has_errors()
    );
}

#[test]
fn downstream_native_modules_expose_an_export_mode() {
    struct DefaultExportModule;
    struct RequireExportModule;

    impl ruau::vm_api::NativeModule for DefaultExportModule {
        fn name(&self) -> &str {
            "default_export"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare default_export: { ping: () -> number }")
        }

        fn build(&self, _builder: &mut dyn ruau::vm_api::ModuleBuilder) {}
    }

    impl ruau::vm_api::NativeModule for RequireExportModule {
        fn name(&self) -> &str {
            "require_export"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare require_export: { ping: () -> number }")
        }

        fn export(&self) -> ruau::vm_api::ModuleExport {
            ruau::vm_api::ModuleExport::Require
        }

        fn build(&self, _builder: &mut dyn ruau::vm_api::ModuleBuilder) {}
    }

    assert_eq!(
        ruau::vm_api::NativeModule::export(&DefaultExportModule),
        ruau::vm_api::ModuleExport::Globals
    );
    assert_eq!(
        ruau::vm_api::NativeModule::export(&RequireExportModule),
        ruau::vm_api::ModuleExport::Require
    );
    assert_eq!(
        ruau::vm_api::ModuleExport::default(),
        ruau::vm_api::ModuleExport::Globals
    );
}

#[test]
fn downstream_host_module_manifest_tracks_export_mode() {
    struct ExportModeModule {
        export: ruau::vm_api::ModuleExport,
    }

    impl ruau::vm_api::NativeModule for ExportModeModule {
        fn name(&self) -> &str {
            "mode"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare mode: { ping: () -> number }")
        }

        fn export(&self) -> ruau::vm_api::ModuleExport {
            self.export
        }

        fn build(&self, builder: &mut dyn ruau::vm_api::ModuleBuilder) {
            use ruau::vm::ModuleBuilderExt;
            builder.leaf_function(
                "ping",
                ruau::vm_api::ModuleBinding::library("mode"),
                |(): ()| 1.0_f64,
            );
        }
    }

    let globals = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .module(std::sync::Arc::new(ExportModeModule {
            export: ruau::vm_api::ModuleExport::Globals,
        }))
        .build()
        .expect("global module surface builds");
    let require = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .module(std::sync::Arc::new(ExportModeModule {
            export: ruau::vm_api::ModuleExport::Require,
        }))
        .build()
        .expect("require module surface builds");

    assert_ne!(
        globals.host_module_manifest_version(),
        require.host_module_manifest_version(),
        "export mode is part of the host-module manifest hash"
    );
}

struct DemoThing;

fn demo_thing_type() -> ruau::vm::HostType {
    HostTypeBuilder::<DemoThing>::new("DemoThing")
        .declaration("declare class DemoThing\nend")
        .build()
}

struct DemoExportModule {
    name: &'static str,
    export: ruau::vm_api::ModuleExport,
    answer: f64,
}

impl ruau::vm_api::NativeModule for DemoExportModule {
    fn name(&self) -> &str {
        self.name
    }

    fn declaration(&self) -> ruau_decl::DeclSource<'_> {
        ruau_decl::DeclSource::Text({
            match self.name {
                "demo_globals" => "declare demo_globals: { answer: () -> number }",
                "demo_require" => "declare demo_require: { answer: () -> number }",
                "demo_both" => {
                    "declare class DemoThing\nend\n\
                 declare demo_both: { answer: () -> number }"
                }
                _ => unreachable!("test module names are fixed"),
            }
        })
    }

    fn export(&self) -> ruau::vm_api::ModuleExport {
        self.export
    }

    fn build(&self, builder: &mut dyn ruau::vm_api::ModuleBuilder) {
        if self.name == "demo_both" {
            ModuleBuilderExt::host_type(builder, demo_thing_type());
            builder.support_chunk("demo.support", b"return { answer = 17 }");
        }
        let answer = self.answer;
        builder.leaf_function(
            "answer",
            ruau::vm_api::ModuleBinding::library(self.name),
            move |(): ()| answer,
        );
    }
}

fn demo_surface() -> ruau::surface::Surface {
    ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .module(std::sync::Arc::new(DemoExportModule {
            name: "demo_globals",
            export: ruau::vm_api::ModuleExport::Globals,
            answer: 3.0,
        }))
        .module(std::sync::Arc::new(DemoExportModule {
            name: "demo_require",
            export: ruau::vm_api::ModuleExport::Require,
            answer: 5.0,
        }))
        .module(std::sync::Arc::new(DemoExportModule {
            name: "demo_both",
            export: ruau::vm_api::ModuleExport::Both,
            answer: 7.0,
        }))
        .build()
        .expect("demo module surface validates")
}

#[test]
fn downstream_demo_module_exercises_export_modes_and_builder_extras() {
    const SOURCE: &[u8] = b"--!strict
local required = require(\"demo_require\")
local both = require(\"demo_both\")
local total: number = demo_globals.answer() + required.answer() + both.answer() + demo_both.answer()
return total
";
    let surface = demo_surface();
    let checked = surface
        .new_checker()
        .check_source(std::str::from_utf8(SOURCE).expect("test source is utf8"));
    let summary = checked.diagnostics().render("demo.luau");
    assert!(!checked.has_errors(), "{summary}");

    let mut vm = surface
        .vm_builder(
            ruau::vm::Ambient::deterministic(0),
            ruau::vm::Limits::unlimited(),
        )
        .build_sandboxed()
        .expect("demo VM builds");
    let chunk = surface
        .compile(SOURCE, &ruau::bytecode::CompileOptions::default())
        .expect("demo source compiles");
    let module = vm.load(&chunk).expect("demo chunk loads");
    let values = vm
        .call_protected(&module, Default::default())
        .expect("demo call is not fatal")
        .expect("demo script succeeds");
    assert_eq!(format!("{values:?}"), "[Number(22.0)]");

    let support_answer: f64 = vm
        .step(|scope| {
            let table = scope
                .named_get(b"demo.support")
                .ok_or_else(|| ruau::vm::RuntimeError::runtime("support chunk missing"))?;
            table.get(scope, "answer")
        })
        .expect("support chunk table is installed");
    assert_eq!(support_answer, 17.0);
}

struct HostConfig(String);

struct HostConfigValue;

impl ruau::vm::ScopedHostFunction for HostConfigValue {
    fn call<'s>(
        &self,
        scope: &ruau::vm::Scope<'s>,
        _args: ruau::vm::MultiValue<'s>,
    ) -> Result<ruau::vm::MultiValue<'s>, ruau::vm::RuntimeError> {
        let value = scope
            .app_data::<HostConfig>()
            .ok_or_else(|| ruau::vm::RuntimeError::runtime("missing HostConfig"))?
            .0
            .clone();
        ruau::vm::IntoLuaMulti::into_lua_multi(value, scope)
    }
}

struct HostEvalModule;

impl ruau::vm_api::NativeModule for HostEvalModule {
    fn name(&self) -> &str {
        "host"
    }

    fn declaration(&self) -> ruau_decl::DeclSource<'_> {
        ruau_decl::DeclSource::Text("declare host: { value: () -> string }")
    }

    fn build(&self, builder: &mut dyn ruau::vm_api::ModuleBuilder) {
        builder.scoped_function(
            "value",
            ruau::vm_api::ModuleBinding::library("host"),
            Box::new(HostConfigValue),
        );
    }
}

fn host_eval_surface() -> ruau::surface::Surface {
    ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .declaration_global("args", "{ name: string }")
        .module(Arc::new(HostEvalModule))
        .build()
        .expect("host eval surface validates")
}

fn current_thread_handle() -> (tokio::runtime::Runtime, tokio::runtime::Handle) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime builds");
    let handle = runtime.handle().clone();
    (runtime, handle)
}

#[test]
fn downstream_evaluator_evaluates_with_args_app_data_and_prints() {
    let (_runtime, handle) = current_thread_handle();
    let host = ruau::host::Evaluator::new(host_eval_surface(), handle);
    let outcome = host
        .eval_blocking(
            "print(\"hello\")\nreturn args.name, host.value()",
            ruau::host::Options::default()
                .chunk_name("host-success.luau")
                .args(serde_json::json!({ "name": "Ada" }))
                .app_data(HostConfig("app-data".to_owned())),
        )
        .expect("eval succeeds");

    assert_eq!(outcome.prints, ["hello"]);
    assert_eq!(outcome.value, Some(serde_json::json!(["Ada", "app-data"])));
}

#[test]
fn downstream_evaluator_reports_compile_errors_with_source_context() {
    let (_runtime, handle) = current_thread_handle();
    let host = ruau::host::Evaluator::new(host_eval_surface(), handle);
    let error = host
        .eval_blocking(
            "local =",
            ruau::host::Options::default().chunk_name("bad.luau"),
        )
        .expect_err("compile fails");

    assert_eq!(error.kind, ruau::host::ErrorKind::Compile);
    assert!(error.line.is_some(), "{error:?}");
    assert!(error.format_pretty().contains("^"));
}

#[test]
fn downstream_evaluator_defaults_to_bounded_untrusted_execution() {
    let (_runtime, handle) = current_thread_handle();
    let host = ruau::host::Evaluator::new(host_eval_surface(), handle);
    let error = host
        .eval_blocking(
            "while true do end",
            ruau::host::Options::default().chunk_name("default-timeout.luau"),
        )
        .expect_err("default options time out a busy script");

    assert!(matches!(
        error.kind,
        ruau::host::ErrorKind::Timeout | ruau::host::ErrorKind::Cancelled
    ));
    assert!(
        error.message.contains("timed out"),
        "unexpected message: {error:?}"
    );
}

#[test]
fn downstream_evaluator_times_out_busy_scripts() {
    let (_runtime, handle) = current_thread_handle();
    let host = ruau::host::Evaluator::new(host_eval_surface(), handle);
    let error = host
        .eval_blocking(
            "while true do end",
            ruau::host::Options::default()
                .chunk_name("timeout.luau")
                .timeout(Duration::from_millis(20)),
        )
        .expect_err("busy script times out");

    assert!(matches!(
        error.kind,
        ruau::host::ErrorKind::Timeout | ruau::host::ErrorKind::Cancelled
    ));
}

#[test]
fn downstream_evaluator_trusted_options_disable_default_timeout() {
    let (_runtime, handle) = current_thread_handle();
    let host = ruau::host::Evaluator::new(host_eval_surface(), handle);
    let cancel = ruau::vm::Cancel::manual();
    let trigger = cancel.clone();
    let started = Instant::now();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(ruau::host::DEFAULT_TIMEOUT + Duration::from_millis(25));
        trigger.cancel();
    });

    let error = host
        .eval_blocking(
            "while true do end",
            ruau::host::Options::trusted()
                .chunk_name("trusted-timeout.luau")
                .cancel(cancel),
        )
        .expect_err("external cancellation stops the trusted unbounded run");

    canceller.join().expect("canceller thread exits");
    assert_eq!(error.kind, ruau::host::ErrorKind::Cancelled);
    assert!(
        started.elapsed() >= ruau::host::DEFAULT_TIMEOUT,
        "trusted options should not install the default timeout: {error:?}"
    );
}

#[test]
fn downstream_evaluator_times_out_many_calls_on_shared_timer() {
    let (_runtime, handle) = current_thread_handle();
    let host = ruau::host::Evaluator::new(host_eval_surface(), handle);

    for index in 0..32 {
        let error = host
            .eval_blocking(
                "while true do end",
                ruau::host::Options::default()
                    .chunk_name(format!("batch-timeout-{index}.luau"))
                    .timeout(Duration::from_millis(5)),
            )
            .expect_err("each busy script times out");

        assert!(matches!(
            error.kind,
            ruau::host::ErrorKind::Timeout | ruau::host::ErrorKind::Cancelled
        ));
    }
}

#[test]
fn downstream_surfaces_accept_declaration_only_globals() {
    let surface = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .declaration_global("args", "{ name: string }")
        .build()
        .expect("declaration-only global validates");

    let checked = surface
        .new_checker()
        .check_source("--!strict\nreturn args.name");
    let summary = checked.diagnostics().render("declaration-only.luau");
    assert!(!checked.has_errors(), "{summary}");

    let mut vm = surface
        .vm_builder(
            ruau::vm::Ambient::deterministic(0),
            ruau::vm::Limits::unlimited(),
        )
        .build_sandboxed()
        .expect("VM builds without installing declaration-only globals");
    let chunk = surface
        .compile(
            b"return args == nil",
            &ruau::bytecode::CompileOptions::default(),
        )
        .expect("compiles");
    let module = vm.load(&chunk).expect("loads");
    let values = vm
        .call_protected(&module, Default::default())
        .expect("not fatal")
        .expect("not a script error");
    assert_eq!(
        format!("{values:?}"),
        "[Boolean(true)]",
        "declaration-only globals are not runtime-installed by the surface"
    );
}

/// `Surface::require_global` obligations resolve against the surface's
/// declared types and are enforced by `new_checker()`-produced checkers:
/// conforming definitions pass, missing or mismatched ones report
/// `required-export` diagnostics, and invalid type text fails registration.
#[test]
fn downstream_surfaces_enforce_required_exports() {
    struct AcmeModule;

    fn answer(_: &ruau::vm::Scope<'_>, (): ()) -> Result<f64, ruau::vm::RuntimeError> {
        Ok(42.0)
    }

    impl ruau::vm_api::NativeModule for AcmeModule {
        fn name(&self) -> &str {
            "acme"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text({
                "type Verdict = number\n\
             declare acme: { answer: () -> Verdict }"
            })
        }

        fn build(&self, builder: &mut dyn ruau::vm_api::ModuleBuilder) {
            use ruau::vm::ModuleBuilderExt;
            builder.scoped_function(
                "answer",
                ruau::vm_api::ModuleBinding::library("acme"),
                ruau::vm::scoped_host_fn(answer),
            );
        }
    }

    let mut surface = ruau::surface::Surface::builder()
        .libraries([])
        .module(std::sync::Arc::new(AcmeModule))
        .build()
        .expect("surface validates");
    surface
        .require_global("decide", "(Verdict) -> (Verdict?, string?)")
        .expect("required type resolves against the surface-declared Verdict alias");

    let strict_config = || {
        ruau::typecheck::checker::Config::with_source_mode(
            ruau::analysis::resolve::AnalysisMode::Strict,
        )
    };

    // A conforming definition passes (and may use the declared module).
    let mut checker = surface.new_checker();
    let checked = checker.check_source_bytes_with_config(
        b"function decide(v: number): (number?, string?)\n\
          \treturn acme.answer() + v, nil\n\
          end",
        strict_config(),
    );
    assert!(
        !checked.has_errors(),
        "{}",
        checked.diagnostics().render("conforming.luau")
    );

    // A module that never defines the global is rejected with the dedicated
    // category and typed payload.
    let mut checker = surface.new_checker();
    let checked = checker.check_source_bytes_with_config(b"local x = 1", strict_config());
    let required: Vec<_> = checked
        .diagnostics()
        .iter()
        .filter(|diagnostic| {
            diagnostic.category == ruau::typecheck::diagnostics::DiagnosticCategory::RequiredExport
        })
        .collect();
    assert_eq!(required.len(), 1, "{:?}", checked.diagnostics());
    assert_eq!(required[0].code(), 1012);
    assert_eq!(
        required[0].payload,
        serde_json::json!({
            "kind": "required-export",
            "name": "decide",
            "required": "(Verdict) -> (Verdict?, string?)",
        })
    );

    // A mismatched definition reports the rendered actual type.
    let mut checker = surface.new_checker();
    let checked = checker.check_source_bytes_with_config(
        b"function decide(v: string): (number?, string?)\n\
          \treturn nil, v\n\
          end",
        strict_config(),
    );
    assert!(
        checked.diagnostics().iter().any(|diagnostic| {
            diagnostic.category == ruau::typecheck::diagnostics::DiagnosticCategory::RequiredExport
                && matches!(
                    &diagnostic.typed_payload,
                    ruau::typecheck::diagnostics::Payload::RequiredExport {
                        name,
                        actual: Some(_),
                        ..
                    } if name == "decide"
                )
        }),
        "{:?}",
        checked.diagnostics()
    );

    // Type text referencing undeclared names fails registration.
    let error = surface
        .require_global("decide", "(Unknowable) -> number")
        .expect_err("undeclared type names are rejected");
    match error {
        ruau::surface::ConfigError::InvalidRequiredGlobal { name, .. } => {
            assert_eq!(name, "decide");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_users_can_run_runner_with_filesystem_module_source() {
    let root = temp_root("filesystem-source");
    write_file(&root.join("modules/dep.luau"), "return 37");
    let runner = filesystem_source_runner(&root);

    let outcome = runner
        .run(ruau::runner::Request::new(
            br#"return require("modules/dep") + 5"#,
            ruau::runner::Budget::with_timeout(std::time::Duration::from_secs(5))
                .expect("future deadline"),
        ))
        .await
        .expect("filesystem-backed request succeeds");

    assert_eq!(
        outcome.values.as_slice(),
        &[ruau::runner::ResultValue::Number(42.0)]
    );
    assert!(runner.report_metadata().module_source_granted);
    remove_dir(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_filesystem_module_source_rejects_root_escape_requires() {
    let root = temp_root("filesystem-source-escape");
    let outside_stem = format!("{}-outside", root.file_name().unwrap().to_string_lossy());
    let outside = root.with_file_name(format!("{outside_stem}.luau"));
    write_file(&outside, "return 99");
    let runner = filesystem_source_runner(&root);
    let source = format!(r#"return require("modules/../../{outside_stem}")"#);

    let report = runner
        .run_report(ruau::runner::Request::new(
            source.as_bytes(),
            ruau::runner::Budget::with_timeout(std::time::Duration::from_secs(5))
                .expect("future deadline"),
        ))
        .await;
    match report.outcome {
        ruau::runner::RequestReportOutcome::Failure {
            error: ruau::runner::RequestError::TypeErrors(diagnostics),
        } => {
            let has_escape_diagnostic = diagnostics.iter().any(|diagnostic| {
                diagnostic.category == ruau::typecheck::diagnostics::DiagnosticCategory::Resolver
                    && diagnostic
                        .payload
                        .get("detail")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|detail| {
                            detail.contains("escapes filesystem root")
                                && !detail.contains(outside.to_string_lossy().as_ref())
                        })
            });
            assert!(
                has_escape_diagnostic,
                "expected root-escape resolver diagnostic, got {diagnostics:?}"
            );
        }
        other => panic!("expected root-escape type error, got {other:?}"),
    }

    remove_file(&outside);
    remove_dir(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_filesystem_module_source_redacts_paths_in_source_diagnostics() {
    let root = temp_root("filesystem-source-redacted");
    write_bytes(&root.join("bad.luau"), &[0xff]);
    let runner = filesystem_source_runner(&root);

    let report = runner
        .run_report(ruau::runner::Request::new(
            br#"return require("bad")"#,
            ruau::runner::Budget::with_timeout(std::time::Duration::from_secs(5))
                .expect("future deadline"),
        ))
        .await;
    match report.outcome {
        ruau::runner::RequestReportOutcome::Failure {
            error: ruau::runner::RequestError::TypeErrors(diagnostics),
        } => {
            let diagnostic = diagnostics
                .iter()
                .find(|diagnostic| {
                    diagnostic.category
                        == ruau::typecheck::diagnostics::DiagnosticCategory::Resolver
                })
                .expect("resolver diagnostic is present");
            assert_eq!(
                diagnostic
                    .payload
                    .get("displayName")
                    .and_then(serde_json::Value::as_str),
                Some("bad.luau")
            );
            let detail = diagnostic
                .payload
                .get("detail")
                .and_then(serde_json::Value::as_str)
                .expect("resolver detail is present");
            assert!(detail.contains("bad.luau"), "{detail}");
            assert!(detail.contains("is not UTF-8"), "{detail}");
            assert!(
                !detail.contains(root.to_string_lossy().as_ref()),
                "{detail}"
            );
        }
        other => panic!("expected redacted source diagnostic, got {other:?}"),
    }

    remove_dir(&root);
}

/// The umbrella-closure guard (API plan Stage 3.9): every type appearing in
/// an exported item's signature must be nameable through `ruau::` paths. The
/// cheap spelling is binding each signature type to an annotated local —
/// these fail to compile (not at runtime) if a path closes over a private or
/// unexported type.
#[test]
fn umbrella_signature_types_are_nameable() {
    fn _assert_into_host_return<T: ruau::vm::IntoHostReturn>() {}

    // session: Vm/VmBuilder signatures.
    let _ambient: Option<ruau::vm::Ambient> = None;
    let _mode: Option<ruau::vm::AmbientMode> = None;
    let _config: Option<ruau::vm::AmbientConfig> = None;
    let _gc: Option<ruau::vm::GcPolicy> = None;
    let _gc_step: Option<ruau::vm::CollectionStepOutcome> = None;
    let _limits: Option<ruau::vm::Limits> = None;
    let _gas_profile: Option<ruau::vm::GasProfile> = None;
    let _gas_profile_entry: Option<ruau::vm::GasProfileEntry> = None;
    let _snapshot: Option<ruau::vm::VmSnapshot> = None;
    let _snapshot_error: Option<ruau::vm::SnapshotError> = None;
    let _cancel: Option<ruau::vm::Cancel> = None;
    let _deadline: Option<ruau::vm::Deadline> = None;
    let _call_options: Option<ruau::vm::CallOptions> = None;
    let _exec_error: Option<ruau::vm::ExecError> = None;
    let _print_sink: Option<ruau::vm::PrintSink> = None;
    let _module: Option<ruau::vm::LoadedModule> = None;
    let _kind: Option<ruau::vm_api::RuntimeErrorKind> = None;
    let _require_kind = ruau::vm_api::RuntimeErrorKind::UnresolvedRequire;
    let _execution_count: fn(&ruau::vm::Vm) -> u64 = ruau::vm::Vm::execution_count;
    // source: module-source family + resolver config.
    let _source: Option<std::sync::Arc<dyn ruau::source::ModuleSource>> = None;
    let _sync_source: Option<Box<dyn ruau::source::SyncModuleSource>> = None;
    let _resolver: Option<Box<dyn ruau::analysis::resolve::config::Resolver>> = None;
    let _read_request: Option<ruau::source::ReadRequest<'static>> = None;
    let _instance_key: Option<ruau::source::InstanceKey> = None;
    let _meta: Option<ruau::source::SourceMetadata> = None;
    let _result: Option<ruau::source::ModuleSourceResult<Vec<u8>>> = None;
    let _error: Option<ruau::source::ModuleSourceError> = None;
    let _mode2: Option<ruau::analysis::resolve::AnalysisMode> = None;
    let _surface_builder = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .analysis_mode(ruau::analysis::resolve::AnalysisMode::Nonstrict);
    let _static_require: Option<ruau::analysis::StaticRequireRequest> = None;
    let _static_require_strings: fn(&ruau::ast::syntax::Stat) -> Vec<String> =
        ruau::analysis::static_require_requests;
    let _static_require_locations: fn(
        &ruau::ast::syntax::Stat,
    ) -> Vec<ruau::analysis::StaticRequireRequest> =
        ruau::analysis::static_require_requests_with_locations;
    let _aliased_source = ruau::source::InMemorySource::new().with_alias("@core/dep", "dep");
    // compile: the safe entry's full signature closure.
    let _chunk: Option<ruau::bytecode::BytecodeChunk> = None;
    let _cerr: Option<ruau::bytecode::CompileError> = None;
    let _ckind: Option<ruau::bytecode::CompileErrorKind> = None;
    let _cloc: Option<ruau::ast::Location> = None;
    let _byte_offset = ruau::ast::Position::new(0, 0).byte_offset("");
    let _byte_range = ruau::ast::Location::new(
        ruau::ast::Position::new(0, 0),
        ruau::ast::Position::new(0, 0),
    )
    .byte_range("");
    let _opts: Option<ruau::bytecode::CompileOptions> = None;
    // types: checking + schema strata.
    let _graph: Option<ruau::analysis::ParseGraphResult> = None;
    let _schema: Option<ruau::typecheck::schema::SchemaModule> = None;
    let _tschema: Option<ruau::typecheck::schema::SchemaType> = None;
    let _sdiag: Option<ruau::typecheck::diagnostics::ModuleDiagnostic> = None;
    let _conformance: Option<ruau::typecheck::checker::ConformanceCheck> = None;
    let _conformance_fingerprint: Option<ruau::typecheck::checker::ConformanceFingerprint> = None;
    // diagnostic: the checker's reporting closure.
    let _diag: Option<ruau::typecheck::diagnostics::Diagnostic> = None;
    let _loc: Option<ruau::typecheck::diagnostics::DiagnosticLocation> = None;
    let _sev: Option<ruau::typecheck::diagnostics::Severity> = None;
    // embed: marshaled values (also `durable`'s state snapshot type).
    let _mv: Option<ruau::vm::MarshaledValue> = None;
    let _mp: Option<ruau::vm::MarshaledPair> = None;
    // Stage Four closures: boundary errors, host unwind, parse fields,
    // checker config fields, fs resolver, diagnostic payload.
    let _load: Option<ruau::vm::LoadError> = None;
    let _build: Option<ruau::vm::VmBuildError> = None;
    let _protected: Option<ruau::vm::ProtectedScriptError> = None;
    // S8: structured traceback frames on the protected/marshaled errors.
    let _frame: Option<ruau::vm::TracebackFrame> = None;
    let _eframe: Option<ruau::vm::TracebackFrame> = None;
    let _merr: Option<ruau::vm::MarshaledScriptError> = None;
    let _eerr: Option<ruau::vm::ExecError> = None;
    let _raw: Option<ruau_vm_api::RawValue> = None;
    let _raw_unwind: Option<ruau_vm_api::Unwind> = None;
    let _unwind: Option<ruau::vm_api::HostUnwind> = None;
    let _host_return: Option<ruau::vm_api::HostReturn> = None;
    let _script_error_field: Option<ruau::vm_api::ScriptErrorField> = None;
    let _str: Option<ruau::vm::Str<'static>> = None;
    let _stashed_table: Option<ruau::vm::StashedTable> = None;
    _assert_into_host_return::<ruau::vm::StashedTable>();
    let _key: Option<ruau::vm::KeyHandle> = None;
    let _pek: Option<ruau::ast::parse::ErrorKind> = None;
    let _comment: Option<ruau::ast::parse::Comment> = None;
    let _hot: Option<ruau::ast::parse::HotComment> = None;
    let _ck: Option<ruau::ast::parse::CommentKind> = None;
    let _acfg: Option<ruau::analysis::resolve::config::AnalysisConfig> = None;
    let _gcfg: Option<ruau::typecheck::checker::GenerationConfig> = None;
    let _payload: Option<ruau::typecheck::diagnostics::Payload> = None;
    let _json: Option<serde_json::Value> = None;
    let _fsres: Option<ruau::fs::FilesystemResolver> = None;
}

/// The `derive` feature wires `#[derive(IntoLua, FromLua)]` through
/// `ruau::vm` (the macro and trait share each name, the serde pattern).
#[cfg(feature = "derive")]
#[test]
fn derive_feature_round_trips_a_plain_struct() {
    use ruau::vm::{FromLua, IntoLua, ScopedValue};

    #[derive(Debug, PartialEq, IntoLua, FromLua)]
    struct Widget {
        name: String,
        count: i64,
    }

    let mut vm = ruau::vm::Vm::builder()
        .ambient(ruau::vm::Ambient::deterministic(0))
        .limits(ruau::vm::Limits::unlimited())
        .runtime_capabilities(ruau::vm::RuntimeCapabilities::default().enable_runtime_compilation())
        .build()
        .expect("vm builds");
    let chunk = ruau::vm::RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(
            b"return 0",
            &ruau::bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
    let module = vm.load(&chunk).expect("load");
    vm.call(&module, Default::default()).expect("run");
    vm.step(|scope| {
        let widget = Widget {
            name: "w".to_owned(),
            count: 3,
        };
        let value = widget.into_lua(scope)?;
        assert!(matches!(value, ScopedValue::Table(_)));
        let back = Widget::from_lua(value, scope)?;
        assert_eq!(
            back,
            Widget {
                name: "w".to_owned(),
                count: 3
            }
        );
        Ok(())
    })
    .expect("scope step");
}

/// A native module whose name, declaration, library, and member names are all
/// runtime-built `String`s passes the surface audit, installs, and serves
/// calls — no `&'static str` (or per-build leak) required anywhere. An audit
/// mismatch reports the runtime module name.
#[test]
fn downstream_surfaces_accept_runtime_built_module_strings() {
    struct RuntimeNamedModule {
        name: String,
        declaration: String,
        library: String,
        member: String,
    }

    fn answer(_: &ruau::vm::Scope<'_>, (): ()) -> Result<f64, ruau::vm::RuntimeError> {
        Ok(42.0)
    }

    impl ruau::vm_api::NativeModule for RuntimeNamedModule {
        fn name(&self) -> &str {
            &self.name
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text(&self.declaration)
        }

        fn build(&self, builder: &mut dyn ruau::vm_api::ModuleBuilder) {
            use ruau::vm::ModuleBuilderExt;
            builder.scoped_function(
                &self.member,
                ruau::vm_api::ModuleBinding::library(self.library.clone()),
                ruau::vm::scoped_host_fn(answer),
            );
        }
    }

    let owner = format!("acme_{}", "widgets");
    let member = String::from("answer");
    let module = RuntimeNamedModule {
        name: owner.clone(),
        declaration: format!("declare {owner}: {{ {member}: () -> number }}"),
        library: owner.clone(),
        member: member.clone(),
    };

    let surface = ruau::surface::Surface::builder()
        .libraries([])
        .module(std::sync::Arc::new(module))
        .build()
        .expect("runtime-named module passes the surface audit");

    let mut vm = surface
        .vm_builder(
            ruau::vm::Ambient::production(0),
            ruau::vm::Limits {
                gas: Some(100_000),
                max_memory_bytes: Some(1 << 20),
                ..ruau::vm::Limits::unlimited()
            },
        )
        .build_sandboxed()
        .expect("vm builds");
    let chunk = surface
        .compile(
            format!("assert({owner}.{member}() == 42, \"wrong answer\") return 0").as_bytes(),
            &ruau::bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
    let loaded = vm.load(&chunk).expect("load");
    vm.call(&loaded, Default::default())
        .expect("the runtime-named member serves calls");

    let mismatched = RuntimeNamedModule {
        name: owner.clone(),
        declaration: format!("declare {owner}: {{ }}"),
        library: owner.clone(),
        member,
    };
    let error = ruau::surface::Surface::builder()
        .libraries([])
        .module(std::sync::Arc::new(mismatched))
        .build()
        .expect_err("a declaration/registration mismatch fails the audit");
    match error {
        ruau::surface::ConfigError::InvalidHostModuleDeclaration { module, .. } => {
            assert_eq!(module, owner)
        }
        other => panic!("expected a host module declaration error, got {other:?}"),
    }
}

/// The always-on `ruau::vm::serde` bridge lets plain serde types cross
/// into scope-borrowed Lua values and back, and owned `MarshaledValue` trees
/// convert to and from `serde_json::Value` — all nameable from the umbrella
/// crate.
#[test]
fn serde_bridge_values_through_the_public_path() {
    use ruau::vm::{
        MarshaledValue, ScopedValue,
        serde::{
            RetainedTableSchema, from_scoped_value, json_to_marshaled, json_to_scoped_value,
            marshaled_to_json, scoped_value_to_json, to_scoped_value,
        },
    };

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum Action {
        Stay,
        Go { dx: i64, dy: i64 },
    }

    let mut vm = ruau::vm::Vm::builder()
        .ambient(ruau::vm::Ambient::deterministic(0))
        .limits(ruau::vm::Limits::unlimited())
        .runtime_capabilities(ruau::vm::RuntimeCapabilities::default().enable_runtime_compilation())
        .build()
        .expect("vm builds");
    let chunk = ruau::vm::RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(
            b"return { kind = 'go', dx = 2, dy = 3 }",
            &ruau::bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
    let module = vm.load(&chunk).expect("load");
    vm.step(|scope| {
        // A script-produced table decodes into the internally tagged enum.
        let main = scope.module_function(&module);
        let value: ScopedValue<'_> = scope.call(main, ())?;
        let action: Action = from_scoped_value(scope, value)?;
        assert_eq!(action, Action::Go { dx: 2, dy: 3 });

        // A host value round-trips through the bridge.
        let encoded = to_scoped_value(scope, &Action::Stay)?;
        let back: Action = from_scoped_value(scope, encoded)?;
        assert_eq!(back, Action::Stay);

        // serde_json::Value is a first-class instantiation.
        let json = serde_json::json!({"a": [1, 2], "b": "x"});
        let encoded = to_scoped_value(scope, &json)?;
        let back: serde_json::Value = from_scoped_value(scope, encoded)?;
        assert_eq!(back, json);

        let faithful_json = serde_json::json!({"delete": null, "empty": [], "object": {}});
        let encoded = json_to_scoped_value(scope, &faithful_json)?;
        assert_eq!(scoped_value_to_json(scope, encoded)?, faithful_json);
        assert_eq!(
            scoped_value_to_json(scope, scope.json_null())?,
            serde_json::json!(null)
        );

        let retained = scope.create_table()?;
        let mut retained_schema = RetainedTableSchema::new();
        retained_schema.write(scope, retained, &json)?;
        let back: serde_json::Value = from_scoped_value(scope, ScopedValue::Table(retained))?;
        assert_eq!(back, json);
        Ok(())
    })
    .expect("scope step");

    // Owned-side conversions reach JSON without re-entering a scope.
    let marshaled = json_to_marshaled(&serde_json::json!({"n": 7})).expect("to marshaled");
    assert_eq!(
        marshaled_to_json(&marshaled).expect("back to json"),
        serde_json::json!({"n": 7})
    );
    let faithful_json = serde_json::json!({"delete": null, "empty": [], "object": {}});
    let marshaled = json_to_marshaled(&faithful_json).expect("to faithful marshaled");
    assert_eq!(
        marshaled_to_json(&marshaled).expect("back to faithful json"),
        faithful_json
    );
    let opaque = MarshaledValue::Opaque("function");
    let error = marshaled_to_json(&opaque).expect_err("opaque is not representable");
    assert_eq!(
        error.message(),
        "an opaque function value is not representable in JSON"
    );
}

/// `Scope::eval_chunk`/`Scope::load_chunk` are reachable through the public
/// embedding surface: a retained session evaluates host-supplied source
/// mid-session in the root chunk's environment, and runtime capabilities without
/// runtime compilation fail closed with a catchable error.
#[test]
fn scope_eval_chunk_runs_through_the_public_surface() {
    use ruau::vm::ScopedValue;

    let mut vm = ruau::vm::Vm::builder()
        .ambient(ruau::vm::Ambient::deterministic(0))
        .limits(ruau::vm::Limits::unlimited())
        .runtime_capabilities(ruau::vm::RuntimeCapabilities::default().enable_runtime_compilation())
        .build()
        .expect("vm builds");
    let chunk = ruau::vm::RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(
            b"base = 40",
            &ruau::bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
    let module = vm.load(&chunk).expect("load");
    vm.step(|scope| {
        let main = scope.module_function(&module);
        let () = scope.call(main, ())?;
        // The eval'd chunk shares the calling environment: it reads `base` and
        // its own write is visible to a later eval in the same session.
        let results = scope.eval_chunk(b"answer = base + 2\nreturn answer", b"=config")?;
        assert!(matches!(
            results.iter().next(),
            Some(ScopedValue::Number(n)) if (n - 42.0).abs() < f64::EPSILON
        ));
        let loaded = scope.load_chunk(b"return answer", b"=reread")?;
        let reread: f64 = scope.call(loaded, ())?;
        assert!((reread - 42.0).abs() < f64::EPSILON);
        Ok(())
    })
    .expect("eval_chunk through the public surface");

    // Capabilities without runtime compilation gate the entry point off.
    let mut gated = ruau::vm::Vm::builder()
        .ambient(ruau::vm::Ambient::deterministic(0))
        .limits(ruau::vm::Limits::unlimited())
        .runtime_capabilities(ruau::vm::RuntimeCapabilities::default())
        .build()
        .expect("gated vm builds");
    gated
        .step(|scope| {
            let error = scope
                .eval_chunk(b"return 1", b"=gated")
                .expect_err("the runtime-compilation gate fails closed");
            assert!(error.message().contains("runtime compilation is disabled"));
            Ok(())
        })
        .expect("the gate error is catchable");
}

#[test]
fn scope_marshal_snapshots_values_through_the_public_surface() {
    use ruau::vm::{MarshaledValue, ScopedValue};

    let mut vm = ruau::vm::Vm::builder()
        .ambient(ruau::vm::Ambient::deterministic(0))
        .limits(ruau::vm::Limits::unlimited())
        .runtime_capabilities(ruau::vm::RuntimeCapabilities::default().enable_runtime_compilation())
        .build()
        .expect("vm builds");
    let chunk = ruau::vm::RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(
            b"return { answer = 42, bytes = buffer.fromstring('bytes') }",
            &ruau::bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
    let module = vm.load(&chunk).expect("load");
    let snapshot = vm
        .step(|scope| {
            let main = scope.module_function(&module);
            let value: ScopedValue<'_> = scope.call(main, ())?;
            scope.marshal(value)
        })
        .expect("scope marshals through public surface");

    let MarshaledValue::Table(pairs) = snapshot else {
        panic!("expected table snapshot, got {snapshot:?}");
    };
    assert!(
        pairs.iter().any(|pair| {
            matches!(
                (&pair.key, &pair.value),
                (MarshaledValue::String(key), MarshaledValue::Number(value))
                    if key == b"answer" && (*value - 42.0).abs() < f64::EPSILON
            )
        }),
        "{pairs:?}"
    );
    assert!(
        pairs.iter().any(|pair| {
            matches!(
                (&pair.key, &pair.value),
                (MarshaledValue::String(key), MarshaledValue::Buffer(value))
                    if key == b"bytes" && value == b"bytes"
            )
        }),
        "{pairs:?}"
    );
}

/// The compile-once, instantiate-many path through the umbrella surface:
/// one `CompiledModule` from a surface feeds preloaded and post-build VMs,
/// and a runtime-capability-mismatched VM rejects it fail-closed.
#[test]
fn downstream_users_can_compile_once_and_instantiate_many() {
    use ruau::{
        bytecode::CompileOptions,
        vm::{
            Ambient, CompiledModule, Library, Limits, LoadError, RuntimeCapabilities, Vm,
            VmBuildError,
        },
    };

    let runtime_capabilities = RuntimeCapabilities::from_libraries(libraries_except(Library::Os));
    let surface = ruau::surface::Surface::builder()
        .libraries(libraries_except(Library::Os))
        .build()
        .expect("surface builds");
    let artifact: CompiledModule = surface
        .compile_module(
            b"assert(6 * 7 == 42, \"wrong answer\") return 0",
            &CompileOptions::default(),
        )
        .expect("surface compiles the artifact");
    assert_eq!(artifact.runtime_capabilities(), &runtime_capabilities);

    // Build-time instantiation through the surface-aligned builder.
    let mut vm = surface
        .vm_builder(Ambient::deterministic(0), Limits::unlimited())
        .preload(&artifact)
        .build()
        .expect("preloaded vm builds");
    let modules = vm.take_preloaded();
    assert_eq!(modules.len(), 1);
    vm.call(&modules[0], Default::default())
        .expect("preloaded module runs");

    // Post-build instantiation of the same artifact into a second VM.
    let mut second = surface
        .vm_builder(Ambient::deterministic(1), Limits::unlimited())
        .build()
        .expect("second vm builds");
    let loaded = second
        .load_compiled(&artifact)
        .expect("the shared artifact loads again");
    second
        .call(&loaded, Default::default())
        .expect("second instance runs");

    // A VM under different runtime capabilities fails closed, at load and at build.
    let mut foreign = Vm::builder()
        .ambient(Ambient::deterministic(0))
        .limits(Limits::unlimited())
        .runtime_capabilities(RuntimeCapabilities::default())
        .build()
        .expect("foreign vm builds");
    assert!(matches!(
        foreign.load_compiled(&artifact),
        Err(LoadError::RuntimeCapabilitiesMismatch { .. })
    ));
    assert!(matches!(
        Vm::builder()
            .ambient(Ambient::deterministic(0))
            .limits(Limits::unlimited())
            .runtime_capabilities(RuntimeCapabilities::default())
            .preload(&artifact)
            .build(),
        Err(VmBuildError::Preload(
            LoadError::RuntimeCapabilitiesMismatch { .. }
        ))
    ));
}
