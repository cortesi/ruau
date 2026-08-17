//! Public API smoke tests for downstream crate use.
#![allow(clippy::tests_outside_test_module)]

use std::{
    fs,
    future::{Future, pending},
    path::{Path, PathBuf},
    process::{Command, id},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ruau::{
    source::{InMemorySource, ModuleId, ModuleName, Source, SourceMetadata, fs::Directory},
    syntax::{
        Stat, Type,
        parse::{Config, parse, parse_with_config},
    },
    typecheck::{GraphChecker, config::EmptyResolver},
    vm::{HostTypeBuilder, module::InstallerExt},
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

fn vm_config(seed: u64) -> ruau::surface::VmConfig {
    ruau::surface::VmConfig::untrusted(
        ruau::vm::Ambient::deterministic(seed),
        ruau::vm::Limits::unlimited(),
    )
}

fn compile_bytes(
    surface: &ruau::surface::Surface,
    source: &[u8],
) -> Result<ruau::bytecode::BytecodeChunk, ruau::bytecode::CompileError> {
    surface.compile(
        &Source::bytes(ModuleId::canonicalized("test-source"), source.to_vec()),
        &ruau::bytecode::CompileOptions::default(),
    )
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
    let default_surface = ruau::surface::Surface::new();
    let default_surface_via_trait = ruau::surface::Surface::default();
    assert_eq!(
        default_surface.libraries(),
        default_surface_via_trait.libraries()
    );
    assert_eq!(
        default_surface.analysis_mode(),
        ruau::typecheck::Mode::Strict
    );

    let _vm = ruau::vm::Vm::builder()
        .runtime_capabilities(runtime_capabilities)
        .ambient(ruau::vm::Ambient::deterministic(0))
        .limits(ruau::vm::Limits::unlimited())
        .trusted_host()
        .build()
        .expect("facade VM builder path works");
}

#[test]
fn surface_vm_config_groups_common_execution_policy() {
    use ruau::{
        surface::VmConfig,
        vm::{Ambient, Limits, SourceModuleExportPolicy, VmSandboxPolicy},
    };

    let deterministic = VmConfig::untrusted(Ambient::deterministic(7), Limits::unlimited());
    assert_eq!(deterministic.ambient(), Ambient::deterministic(7));
    assert_eq!(deterministic.sandbox_policy(), VmSandboxPolicy::Untrusted);
    assert_eq!(
        deterministic.source_module_export_policy(),
        SourceModuleExportPolicy::Mutable
    );
    assert_eq!(deterministic.limits().gas, None);
    assert_eq!(deterministic.limits().max_memory_bytes, None);

    let metered = VmConfig::untrusted(
        Ambient::deterministic(9),
        Limits::metered(1_000_000, 16 * 1024 * 1024),
    );
    assert_eq!(metered.ambient(), Ambient::deterministic(9));
    assert_eq!(metered.sandbox_policy(), VmSandboxPolicy::Untrusted);
    assert_eq!(metered.limits().gas, Some(1_000_000));
    assert_eq!(metered.limits().max_memory_bytes, Some(16 * 1024 * 1024));
    assert_eq!(metered.limits().max_string_bytes, Some(4 * 1024 * 1024));
    assert_eq!(metered.limits().max_buffer_bytes, Some(4 * 1024 * 1024));
    assert_eq!(metered.limits().max_pack_bytes, Some(4 * 1024 * 1024));
    assert_eq!(metered.limits().max_table_elements, Some(1024 * 1024));
    assert_eq!(
        metered.limits().max_runtime_compile_source_bytes,
        Some(4 * 1024 * 1024)
    );
    assert_eq!(
        metered.limits().max_runtime_compile_instructions,
        Some(2 * 1024 * 1024)
    );
    assert_eq!(
        metered.limits().max_runtime_compile_bytecode_bytes,
        Some(4 * 1024 * 1024)
    );

    let production = VmConfig::untrusted(
        Ambient::production(11),
        Limits::metered(2_000_000, 32 * 1024 * 1024),
    );
    assert_eq!(production.ambient(), Ambient::production(11));
    assert_eq!(production.limits().gas, Some(2_000_000));

    let trusted = VmConfig::trusted_host(Ambient::deterministic(13), Limits::unlimited());
    assert_eq!(trusted.sandbox_policy(), VmSandboxPolicy::TrustedHost);

    let overridden = VmConfig::untrusted(Ambient::deterministic(0), Limits::unlimited())
        .with_ambient(Ambient::deterministic(17))
        .with_limits(Limits::metered(3_000_000, 64 * 1024 * 1024))
        .with_source_module_export_policy(SourceModuleExportPolicy::DeepFrozen);
    assert_eq!(overridden.ambient(), Ambient::deterministic(17));
    assert_eq!(overridden.limits().gas, Some(3_000_000));
    assert_eq!(overridden.sandbox_policy(), VmSandboxPolicy::Untrusted);
    assert_eq!(
        overridden.source_module_export_policy(),
        SourceModuleExportPolicy::DeepFrozen
    );
}

#[test]
fn surface_vm_config_applies_source_module_export_policy() {
    use ruau::{
        surface::{Surface, VmConfig},
        vm::{Ambient, CallOptions, Limits, SourceModuleExportPolicy, ValueSnapshot},
    };

    let source = Arc::new(InMemorySource::new().with_module(
        ModuleId::new("exports"),
        "local exports = { nested = {} } exports.self = exports return exports",
    ));
    let surface = Surface::builder()
        .module_source(source)
        .build()
        .expect("surface validates");
    let chunk = compile_bytes(
        &surface,
        br#"
local exports = require("exports")
local nested_mutated = pcall(function() exports.nested.value = 1 end)
return table.isfrozen(exports), table.isfrozen(exports.nested), exports.self == exports,
    nested_mutated
"#,
    )
    .expect("probe compiles");
    let config = VmConfig::untrusted(Ambient::deterministic(0), Limits::unlimited())
        .with_source_module_export_policy(SourceModuleExportPolicy::DeepFrozen);
    let mut vm = surface.vm_builder(&config).build().expect("VM builds");
    let module = vm.load(&chunk).expect("probe loads");

    assert_eq!(
        vm.exec(&module, CallOptions::new()).expect("probe runs"),
        vec![
            ValueSnapshot::Boolean(true),
            ValueSnapshot::Boolean(true),
            ValueSnapshot::Boolean(true),
            ValueSnapshot::Boolean(false),
        ]
    );
}

#[test]
fn surface_vm_config_drives_sandbox_and_call_limit_overlays() {
    use ruau::{
        surface::{Surface, VmConfig},
        vm::{Ambient, CallOptions, ExecError, Limits, ValueSnapshot},
    };

    let surface = Surface::new();
    let mutate_library = compile_bytes(
        &surface,
        b"string.stage_nine = function() return 42 end return string.stage_nine()",
    )
    .expect("library-mutation source compiles");

    let mut sandboxed = surface
        .vm_builder(&vm_config(0))
        .build()
        .expect("sandboxed VM builds");
    let module = sandboxed.load(&mutate_library).expect("chunk loads");
    let error = sandboxed
        .exec(&module, CallOptions::new())
        .expect_err("sandboxed defaults reject shared-library mutation");
    assert!(matches!(
        error,
        ExecError::Script(ref script)
            if script.kind() == ruau::vm::RuntimeErrorKind::Runtime
    ));

    let mut convenience_sandboxed = surface
        .vm_builder(&VmConfig::untrusted(
            Ambient::deterministic(0),
            Limits::unlimited(),
        ))
        .build()
        .expect("convenience sandboxed VM builds");
    let module = convenience_sandboxed
        .load(&mutate_library)
        .expect("chunk loads");
    assert!(
        convenience_sandboxed
            .exec(&module, CallOptions::new())
            .is_err(),
        "the convenience builder installs the untrusted-code sandbox"
    );

    let mut trusted = surface
        .vm_builder(&VmConfig::trusted_host(
            Ambient::deterministic(0),
            Limits::unlimited(),
        ))
        .build()
        .expect("trusted VM builds");
    let module = trusted.load(&mutate_library).expect("chunk loads");
    let values = trusted
        .exec(&module, CallOptions::new())
        .expect("trusted host VM can mutate its own library table");
    assert_eq!(values, vec![ValueSnapshot::Number(42.0)]);

    let loop_chunk = compile_bytes(
        &surface,
        b"local total = 0 for i = 1, 1000 do total += i end return total",
    )
    .expect("loop source compiles");
    let mut metered = surface
        .vm_builder(&vm_config(0).with_limits(Limits::metered(100_000, 16 * 1024 * 1024)))
        .build()
        .expect("metered VM builds");
    let module = metered.load(&loop_chunk).expect("loop loads");
    let limited = metered.exec(
        &module,
        CallOptions::new().limits(Limits {
            gas: Some(10),
            ..Limits::unlimited()
        }),
    );
    assert!(matches!(limited, Err(ExecError::Script(_))));

    let values = metered
        .exec(&module, CallOptions::new())
        .expect("default limits are restored after the per-call overlay");
    assert_eq!(values, vec![ValueSnapshot::Number(500_500.0)]);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn downstream_users_can_drive_async_vm_entries_on_a_local_executor() {
    use ruau::{
        surface::Surface,
        vm::{Ambient, CallOptions, Limits, LocalExecutor, ValueSnapshot},
    };

    let surface = Surface::new();
    let chunk = compile_bytes(&surface, b"return 21 * 2").expect("source compiles");
    let mut vm = surface
        .vm_builder(&ruau::surface::VmConfig::untrusted(
            Ambient::deterministic(0),
            Limits::unlimited(),
        ))
        .build()
        .expect("VM builds");
    let module = vm.load(&chunk).expect("module loads");

    let executor = LocalExecutor::new().expect("local executor builds");
    let values = executor
        .run(vm.exec_async(&module, CallOptions::new()))
        .expect("async entry runs");
    assert_eq!(values, vec![ValueSnapshot::Number(42.0)]);

    let values = ruau::vm::run_local(vm.exec_async(&module, CallOptions::new()))
        .expect("temporary local executor builds")
        .expect("async entry runs");
    assert_eq!(values, vec![ValueSnapshot::Number(42.0)]);
}

#[test]
fn downstream_sources_drive_surface_check_compile_and_load_identity() {
    let surface = ruau::surface::Surface::new();
    let source = Source::text(
        ModuleId::new("source/main.luau"),
        "--!strict\nlocal value: number = 41\nreturn value + 1",
    );

    let prepared = surface.prepare(source).expect("source prepares");
    assert!(
        !prepared.diagnostics().has_errors(),
        "{}",
        prepared
            .diagnostics()
            .render(prepared.source().display_name())
    );
    assert_eq!(prepared.load_name().as_slice(), b"@source/main.luau");
    assert_eq!(
        prepared.runtime_capabilities(),
        surface.runtime_capabilities()
    );

    let mut vm = surface
        .vm_builder(&vm_config(0))
        .build()
        .expect("source VM builds");
    let module = vm
        .load_named(prepared.chunk(), &prepared.load_name())
        .expect("source load name is accepted");
    let values = vm
        .call(&module, Default::default())
        .expect("source script runs");
    assert_eq!(format!("{values:?}"), "[Number(42.0)]");

    let byte_source = Source::bytes(
        ModuleId::new("source/bytes.luau"),
        b"--!strict\nreturn \"\xff\"".as_slice(),
    );
    assert_eq!(byte_source.as_str(), None);
    let prepared = surface.prepare(byte_source).expect("byte source prepares");
    assert!(
        !prepared.diagnostics().has_errors(),
        "{}",
        prepared
            .diagnostics()
            .render(prepared.source().display_name())
    );
    assert_eq!(prepared.source().as_str(), None);
    assert_eq!(prepared.load_name().as_slice(), b"@source/bytes.luau");

    for (id, expected_load_name) in [
        ("=display-name", b"=display-name".as_slice()),
        ("@already/path.luau", b"@already/path.luau".as_slice()),
    ] {
        let prepared = surface
            .prepare(Source::text(ModuleId::new(id), "return 1"))
            .expect("marked load name prepares");
        assert_eq!(prepared.load_name().as_slice(), expected_load_name);
    }
}

#[test]
fn prepared_script_run_in_uses_names_requesters_and_call_options() {
    use ruau::{
        surface::{PreparedRunError, Surface},
        vm::{CallOptions, ExecError, Limits, ValueSnapshot},
    };

    let modules = Arc::new(
        InMemorySource::new().with_module(ModuleId::new("pkg/dep"), "return { value = 41 }"),
    );
    let surface = Surface::builder()
        .module_source(modules)
        .build()
        .expect("surface builds");

    let prepared = surface
        .prepare(Source::text(
            ModuleId::new("pkg/main"),
            "--!strict\nlocal dep = require('./dep')\nreturn dep.value + 1",
        ))
        .expect("root source prepares");
    let mut vm = surface
        .vm_builder(&vm_config(0))
        .build()
        .expect("VM builds");
    let values = prepared.run(&mut vm).expect("prepared root runs");
    assert_eq!(values, vec![ValueSnapshot::Number(42.0)]);

    let traceback = surface
        .prepare(Source::text(
            ModuleId::new("trace/main.luau"),
            "local function fail()\n    error('boom')\nend\nfail()",
        ))
        .expect("traceback source prepares");
    let error = traceback
        .run(&mut vm)
        .expect_err("script error is reported");
    let PreparedRunError::Exec(ExecError::Script(script)) = error else {
        panic!("expected script execution error, got {error:?}");
    };
    let trace = script.traceback().expect("traceback is captured");
    assert!(trace.contains("trace/main.luau"), "{trace}");
    assert!(!trace.contains("[string"), "{trace}");

    let limited = surface
        .prepare(Source::text(
            ModuleId::new("limits/main.luau"),
            "local total = 0 for i = 1, 1000 do total += i end return total",
        ))
        .expect("limited source prepares");
    let error = limited
        .run_with_options(
            &mut vm,
            CallOptions::new().limits(Limits {
                gas: Some(10),
                ..Limits::unlimited()
            }),
        )
        .expect_err("per-call gas overlay is honored");
    assert!(matches!(
        error,
        PreparedRunError::Exec(ExecError::Script(_))
    ));

    let loaded = limited.load(&mut vm).expect("prepared source loads");
    let values = vm
        .exec(&loaded, CallOptions::new())
        .expect("lower-level exec still works with loaded prepared module");
    vm.unload(loaded);
    assert_eq!(values, vec![ValueSnapshot::Number(500_500.0)]);
}

#[test]
fn surface_prepare_rejects_type_errors_and_names_diagnostics() {
    let surface = ruau::surface::Surface::new();
    let source = Source::text(
        ModuleId::new("source/type_error.luau"),
        "--!strict\nlocal value: number = 'oops'\nreturn value",
    );

    let error = surface
        .prepare(source)
        .expect_err("type errors reject prepare");

    assert_eq!(
        error.diagnostic_policy(),
        Some(ruau::surface::PrepareDiagnosticPolicy::RejectErrors)
    );
    assert!(error.compile_error().is_none());
    assert!(error.diagnostics().has_errors(), "{error:?}");
    assert!(error.to_string().contains("source/type_error.luau"));
    assert!(
        error
            .diagnostics()
            .render(error.script_source().display_name())
            .contains("source/type_error.luau")
    );
}

#[test]
fn surface_prepare_preserves_warnings_until_policy_rejects_issues() {
    let surface = ruau::surface::Surface::new();
    let mut config = ruau::typecheck::Config::with_source_mode(ruau::typecheck::Mode::Nonstrict);
    config.analysis.set_type_errors(false);
    let source = Source::text(
        ModuleId::new("source/warning.luau"),
        "local value: number = 'warning'",
    );

    let prepared = surface
        .prepare_with_options(
            source.clone(),
            ruau::surface::PrepareOptions::new().with_check_config(config.clone()),
        )
        .expect("warning-only diagnostics do not block default preparation");
    assert!(prepared.diagnostics().has_issues());
    assert!(!prepared.diagnostics().has_errors());
    assert_eq!(prepared.diagnostics().warning_count(), 1);

    let error = surface
        .prepare_with_options(
            source,
            ruau::surface::PrepareOptions::new()
                .with_check_config(config)
                .reject_issues(),
        )
        .expect_err("stricter policy rejects warning-only diagnostics");
    assert_eq!(
        error.diagnostic_policy(),
        Some(ruau::surface::PrepareDiagnosticPolicy::RejectIssues)
    );
    assert!(error.diagnostics().has_issues());
    assert!(!error.diagnostics().has_errors());
}

#[test]
fn surface_prepare_reports_compile_errors_with_source_name() {
    let surface = ruau::surface::Surface::new();
    let source = Source::text(
        ModuleId::new("source/compile_error.luau"),
        "local function broken(",
    );

    let error = surface
        .prepare_with_options(
            source,
            ruau::surface::PrepareOptions::new().allow_diagnostics(),
        )
        .expect_err("accepted diagnostics still compile through the compiler");

    assert!(error.compile_error().is_some(), "{error:?}");
    assert!(error.to_string().contains("source/compile_error.luau"));
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

fn filesystem_source_executor(root: &Path) -> ruau::executor::Executor {
    let source = Arc::new(Directory::new(root).expect("filesystem root validates"));
    let surface = ruau::surface::Surface::builder()
        .module_source(source)
        .build()
        .expect("filesystem-backed surface validates");

    ruau::executor::Executor::builder()
        .aggregate_resource_limits(ruau::executor::AggregateResourceLimits::unlimited())
        .surface(surface)
        .ambient(ruau::vm::Ambient::production(0))
        .limits(ruau::vm::Limits::metered(100_000, 1 << 20))
        .lane_count(1)
        .lane_admission_limits(ruau::executor::AdmissionLimits {
            max_in_flight: 1,
            max_in_flight_per_tenant: 1,
            max_queued: 1,
            max_queued_per_tenant: 1,
            max_total: 2,
        })
        .features(ruau::vm::ExecutionFeatures::all_off())
        .max_source_bytes(1024)
        .build()
        .expect("executor validates")
}

#[test]
fn downstream_retained_session_builder_path_is_live() {
    let _builder: ruau::vm::VmBuilder = ruau::vm::Vm::builder();
}

#[test]
fn downstream_users_can_parse_declaration_syntax_through_umbrella() {
    let parsed = parse_with_config(
        "declare module: { ping: (message: string?) -> string }\n",
        &Config {
            allow_declaration_syntax: true,
            ..Config::default()
        },
    );

    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    let Stat::Block { body, .. } = parsed.root else {
        panic!("expected parsed root block");
    };
    let Stat::DeclareGlobal { declared_type, .. } = &body[0] else {
        panic!("expected declaration global");
    };
    assert!(matches!(declared_type.as_ref(), Type::Table { .. }));
}

#[test]
fn downstream_users_can_parse() {
    let result = parse("return require(script.Module)");
    assert!(result.errors.is_empty());
    assert!(matches!(result.root, Stat::Block { .. }));
}

#[test]
fn downstream_users_can_trace_and_parse_module_graphs() {
    let sources = InMemorySource::new()
        .with_module(ModuleId::new("main"), r#"return require("dep")"#)
        .with_module(ModuleId::new("dep"), "return {}");
    let config = EmptyResolver;
    let mut checked = GraphChecker::new(&sources, &config);

    let graph =
        block_on_test(checked.check_graph("main")).expect("umbrella graph checker is unlimited");
    assert!(!graph.has_errors(), "{:?}", graph.diagnostics());

    assert_eq!(
        graph
            .build_queue()
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
            ModuleId::new("dep"),
            "--!strict\nexport type DepRow = { name: string }\nreturn 3",
        )
        .with_module(
            ModuleId::new("main"),
            "--!strict\n\
             local dep = require(\"dep\")\n\
             export type Handler = (number) -> string\n\
             export type Row = { id: number }\n\
             return function(value: number): string return tostring(value + dep) end",
        );
    let config = EmptyResolver;
    let mut frontend = ruau::typecheck::GraphChecker::new(&sources, &config);

    block_on_test(frontend.check_graph("main")).expect("umbrella graph checker is unlimited");
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
            ModuleId::new("dep"),
            "--!strict\nlocal value: number = \"bad\"\nreturn value",
        )
        .with_metadata(ModuleId::new("dep"), SourceMetadata::new("tenant/dep.luau"))
        .with_module(ModuleId::new("main"), "--!strict\nreturn require(\"dep\")");
    let config = EmptyResolver;
    let mut frontend = ruau::typecheck::GraphChecker::new(&sources, &config);

    block_on_test(frontend.check_graph("main")).expect("umbrella graph checker is unlimited");
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
        ruau::typecheck::DiagnosticCategory::Resolver
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
            Ok(ruau::vm::HostReturn {
                values: vec![ruau::vm::OwnedValue::Integer(value)],
            })
        });
    assert_string_conversion::<String>();
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_module_builder_helpers_cover_closures_methods_and_async_locations() {
    struct HelperModule;

    impl ruau::vm::NativeModule for HelperModule {
        fn name(&self) -> &str {
            "helper"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text(
                "declare helper: { join: (string, string) -> string, line: () -> number }",
            )
        }

        fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
            use ruau::vm::{MethodArgs, module::InstallerExt};

            builder.scoped_function_fn(
                "join",
                ruau::vm::ModuleBinding::library("helper"),
                |_scope, method: MethodArgs<String, String>| {
                    let (receiver, arg) = method.into_parts();
                    Ok(format!("{receiver}:{arg}"))
                },
            );

            builder.async_function_fn(
                "line",
                ruau::vm::ModuleBinding::library("helper"),
                |ctx, (): ()| async move {
                    let location = ctx
                        .caller_location(0)
                        .await?
                        .ok_or_else(|| ruau::vm::RuntimeError::runtime("missing caller"))?;
                    Ok(ruau::vm::HostReturn {
                        values: vec![ruau::vm::OwnedValue::Integer(i64::from(location.line))],
                    })
                },
            );
        }
    }

    let surface = ruau::surface::Surface::builder()
        .module(std::sync::Arc::new(HelperModule))
        .build()
        .expect("surface validates");
    let chunk = compile_bytes(
        &surface,
        b"local joined = helper.join(\"left\", \"right\")\n\
              local line = helper.line()\n\
              return joined, line",
    )
    .expect("source compiles");
    let mut vm = surface
        .vm_builder(&ruau::surface::VmConfig::untrusted(
            ruau::vm::Ambient::deterministic(0),
            ruau::vm::Limits::unlimited(),
        ))
        .build()
        .expect("VM builds");
    let module = vm.load(&chunk).expect("module loads");

    let values = vm
        .exec_async(&module, ruau::vm::CallOptions::new())
        .await
        .expect("script runs");
    assert_eq!(
        values,
        vec![
            ruau::vm::ValueSnapshot::String(b"left:right".to_vec()),
            ruau::vm::ValueSnapshot::Integer(2),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_async_hosts_return_stashed_tables() {
    struct VerberModule;

    impl ruau::vm::NativeModule for VerberModule {
        fn name(&self) -> &str {
            "verber"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text({
                "declare verber: { make: () -> { answer: number, label: string } }"
            })
        }

        fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
            use ruau::vm::{IntoHostReturn, module::InstallerExt};

            builder.async_function(
                "make",
                ruau::vm::ModuleBinding::library("verber"),
                ruau::vm::async_host_fn(|ctx: ruau::vm::AsyncHostContext, (): ()| async move {
                    let table = ctx
                        .scope(|scope| {
                            let table = scope.create_table()?;
                            table.set(scope, "answer", 42.0)?;
                            table.set(scope, "label", "built")?;
                            scope.stash_table(table)
                        })
                        .await?;
                    Ok(ruau::vm::HostReturn {
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
        .vm_builder(&ruau::surface::VmConfig::untrusted(
            ruau::vm::Ambient::production(0),
            ruau::vm::Limits::metered(100_000, 1 << 20),
        ))
        .build()
        .expect("vm builds");
    let chunk = compile_bytes(
        &surface,
        b"local t = verber.make()\n\
              assert(t.answer == 42, \"wrong answer\")\n\
              assert(t.label == \"built\", \"wrong label\")\n\
              return 0",
    )
    .expect("compile");
    let loaded = vm.load(&chunk).expect("load");
    if let Err(error) = vm.call_async(&loaded, Default::default()).await {
        let detail = match error.error {
            ruau_vm::RawValue::String(handle) => {
                let bytes = vm.heap().string(handle).expect("error string").bytes();
                String::from_utf8_lossy(bytes).into_owned()
            }
            other => format!("{other:?}"),
        };
        panic!("stashed table returns through async host: {detail}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_users_can_run_with_curated_executor_surface() {
    use std::{sync::Arc, time::Duration};

    let source = Arc::new(
        ruau::source::InMemorySource::new().with_module(ModuleId::new("dep"), "return 37"),
    );
    let surface = ruau::surface::Surface::builder()
        .libraries([])
        .module_source(source)
        .build()
        .expect("surface validates");

    let executor = ruau::executor::Executor::builder()
        .aggregate_resource_limits(ruau::executor::AggregateResourceLimits::unlimited())
        .surface(surface)
        .ambient(ruau::vm::Ambient::production(0))
        .limits(ruau::vm::Limits::metered(100_000, 1 << 20))
        .lane_count(2)
        .lane_admission_limits(ruau::executor::AdmissionLimits {
            max_in_flight: 2,
            max_in_flight_per_tenant: 1,
            max_queued: 2,
            max_queued_per_tenant: 1,
            max_total: 4,
        })
        .features(ruau::vm::ExecutionFeatures::all_off())
        .max_source_bytes(1024)
        .build()
        .expect("executor validates");
    assert_eq!(executor.lane_count(), 2);
    assert_eq!(executor.lane_metrics().lanes, 2);

    let outcome = executor
        .run(ruau::executor::Request::new(
            ruau::executor::TenantId(0),
            br#"return require("dep") + 5"#,
            ruau::executor::RunControl::with_timeout(Duration::from_secs(5))
                .expect("future deadline"),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(
        outcome.values.as_slice(),
        &[ruau::vm::ValueSnapshot::Number(42.0)]
    );
    assert!(executor.report_metadata().module_source_granted);
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_users_can_run_multi_tenant_executor_paths() {
    let surface = ruau::surface::Surface::new();
    let executor = ruau::executor::Executor::builder()
        .aggregate_resource_limits(ruau::executor::AggregateResourceLimits::unlimited())
        .surface(surface)
        .ambient(ruau::vm::Ambient::production(0))
        .limits(ruau::vm::Limits::metered(100_000, 1 << 20))
        .lane_count(2)
        .lane_admission_limits(ruau::executor::AdmissionLimits {
            max_in_flight: 2,
            max_in_flight_per_tenant: 1,
            max_queued: 2,
            max_queued_per_tenant: 1,
            max_total: 4,
        })
        .features(ruau::vm::ExecutionFeatures::all_off())
        .max_source_bytes(1024)
        .build()
        .expect("executor validates");

    let alpha = ruau::executor::TenantId(1);
    let beta = ruau::executor::TenantId(2);
    let alpha_source = b"return 11";
    let beta_source = b"return 22";

    let alpha_report = executor
        .run_report(
            ruau::executor::Request::new(
                ruau::executor::TenantId(0),
                alpha_source,
                ruau::executor::RunControl::with_timeout(std::time::Duration::from_secs(5))
                    .expect("future deadline"),
            )
            .with_tenant(alpha),
        )
        .await;
    let beta_report = executor
        .run_report(
            ruau::executor::Request::new(
                ruau::executor::TenantId(0),
                beta_source,
                ruau::executor::RunControl::with_timeout(std::time::Duration::from_secs(5))
                    .expect("future deadline"),
            )
            .with_tenant(beta),
        )
        .await;

    assert_eq!(alpha_report.tenant, alpha);
    assert_eq!(beta_report.tenant, beta);
    match alpha_report.result {
        Ok(values) => {
            assert_eq!(values, vec![ruau::vm::ValueSnapshot::Number(11.0)]);
        }
        other => panic!("alpha tenant should succeed, got {other:?}"),
    }
    match beta_report.result {
        Ok(values) => {
            assert_eq!(values, vec![ruau::vm::ValueSnapshot::Number(22.0)]);
        }
        other => panic!("beta tenant should succeed, got {other:?}"),
    }

    let alpha_totals = executor.tenant_resource_totals(alpha);
    let beta_totals = executor.tenant_resource_totals(beta);
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
    assert_eq!(executor.lane_metrics().lanes, 2);
}

#[test]
fn downstream_users_can_reuse_surface_checker_for_schema_checks() {
    let sources = InMemorySource::new()
        .with_module(
            ModuleId::new("dep"),
            "--!strict\nexport type Dep = { value: number }\nreturn 3",
        )
        .with_module(
            ModuleId::new("main"),
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
    let mut frontend =
        ruau::typecheck::GraphChecker::with_checker(&sources, &config, surface.new_checker());

    block_on_test(frontend.check_graph("main")).expect("umbrella graph checker is unlimited");
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
    let surface = ruau::surface::Surface::new();
    let mut checker = surface.new_checker();

    let checked = checker.check_source_with_config(
        r#"--!strict
return require("dep")
"#,
        ruau::typecheck::Config::with_source_mode(ruau::typecheck::Mode::Strict),
    );
    let summary = checked.diagnostics().render("sourceless.luau");

    assert!(checked.has_errors(), "{summary}");
    assert!(
        checked
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.category
                == ruau::typecheck::DiagnosticCategory::UnknownSymbol),
        "{summary}"
    );
}

#[test]
fn downstream_surface_module_source_changes_update_require_without_rebuilding_declarations() {
    let source =
        std::sync::Arc::new(InMemorySource::new().with_module(ModuleId::new("dep"), "return 1"));
    let mut surface = ruau::surface::Surface::new();

    assert!(surface.new_checker().builtins().global("require").is_none());
    surface.replace_module_source(Some(source));
    assert!(surface.new_checker().builtins().global("require").is_some());
    surface.replace_module_source(None);
    assert!(surface.new_checker().builtins().global("require").is_none());
}

#[test]
fn downstream_surface_checks_source_text_with_surface_mode() {
    let surface = ruau::surface::Surface::builder()
        .analysis_mode(ruau::typecheck::Mode::Nonstrict)
        .build()
        .expect("surface validates");

    let checked = surface.check(
        &Source::text(
            ModuleId::canonicalized("nonstrict"),
            "local x: { foo: string }? = nil\nlocal y = x.foo\nlocal _ = y",
        ),
        ruau::surface::CheckOptions::default(),
    );

    assert_eq!(checked.mode(), ruau::typecheck::Mode::Nonstrict);
    assert!(
        !checked.has_errors(),
        "{}",
        checked.diagnostics().render("nonstrict.luau")
    );
}

#[test]
fn downstream_surface_check_config_override_wins_over_surface_mode() {
    let surface = ruau::surface::Surface::builder()
        .analysis_mode(ruau::typecheck::Mode::Nonstrict)
        .build()
        .expect("surface validates");

    let checked = surface.check(
        &Source::text(
            ModuleId::canonicalized("strict"),
            "local x: { foo: string }? = nil\nlocal y = x.foo\nlocal _ = y",
        ),
        ruau::surface::CheckOptions::default().with_config(
            ruau::typecheck::Config::with_source_mode(ruau::typecheck::Mode::Strict),
        ),
    );

    assert_eq!(checked.mode(), ruau::typecheck::Mode::Strict);
    assert!(
        checked.has_errors(),
        "strict override should reject nil property read"
    );
}

#[test]
fn downstream_surfaces_contextually_require_one_root_return() {
    let surface = ruau::surface::Surface::builder()
        .require_return("(number) -> string")
        .build()
        .expect("required return surface validates");

    let valid = surface.check(
        &Source::text(
            ModuleId::canonicalized("valid"),
            "return function(value) return tostring(value) end",
        ),
        ruau::surface::CheckOptions::default(),
    );
    assert!(
        !valid.has_errors(),
        "{}",
        valid.diagnostics().render("valid.luau")
    );

    for source in [
        "return function(value) return value + 'bad' end",
        "local value = 1",
        "return function(value) return tostring(value) end, true",
    ] {
        let invalid = surface.check(
            &Source::text(ModuleId::canonicalized("invalid"), source),
            ruau::surface::CheckOptions::default(),
        );
        assert!(
            invalid.has_errors(),
            "required root return accepted {source:?}"
        );
    }
}

#[test]
fn downstream_graph_required_return_applies_only_to_the_root() {
    let sources =
        Arc::new(InMemorySource::new().with_module(ModuleId::new("app/dep"), "return 41"));
    let surface = ruau::surface::Surface::builder()
        .module_source(sources)
        .require_return("(number) -> string")
        .build()
        .expect("required graph surface validates");
    let root = Source::text(
        ModuleId::new("app/main"),
        "local dep = require('./dep')\nreturn function(value) return tostring(value + dep) end",
    );

    let graph = surface
        .check_graph_ready(ruau::surface::GraphRoot::overlay(&root), Default::default())
        .expect("graph is within default limits");
    assert!(!graph.has_errors(), "{}", graph.diagnostics().render());
}

#[test]
fn downstream_checker_extracts_schema_from_checked_module() {
    let mut checker = ruau::typecheck::Checker::new();
    let checked = checker.check_source(
        "--!strict\n\
         export type Handler = (number) -> string\n\
         return function(value: number): string return tostring(value) end",
    );

    let schema = ruau::typecheck::schema::extract_module(checker.arena(), &checked);

    assert!(!schema.has_errors(), "{:?}", schema.diagnostics);
    assert_eq!(schema.exported_functions().count(), 1);
    assert_eq!(schema.return_types.len(), 1);
}

#[test]
fn downstream_frontend_surface_checker_types_native_require_exports() {
    struct NativeRequireModule;

    impl ruau::vm::NativeModule for NativeRequireModule {
        fn name(&self) -> &str {
            "native"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("declare native: { answer: () -> number }")
        }

        fn export(&self) -> ruau::vm::ModuleExport {
            ruau::vm::ModuleExport::Require
        }

        fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
            builder.leaf_function(
                "answer",
                ruau::vm::ModuleBinding::library("native"),
                |(): ()| 42.0_f64,
            );
        }
    }

    let sources = InMemorySource::new().with_module(
        ModuleId::new("main"),
        "--!strict\nlocal native = require(\"native\")\nlocal answer: number = native.answer()\nreturn answer\n",
    );
    let surface = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .module(std::sync::Arc::new(NativeRequireModule))
        .build()
        .expect("native require surface validates");
    let config = EmptyResolver;
    let mut frontend = GraphChecker::with_checker(&sources, &config, surface.new_checker());

    let graph =
        block_on_test(frontend.check_graph("main")).expect("umbrella graph checker is unlimited");
    let diagnostics = graph.diagnostics();

    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(
        !frontend
            .checked_module(&ModuleName::from("main"))
            .expect("main checked")
            .has_errors()
    );
}

#[test]
fn surface_check_module_graph_reports_dependency_type_errors_with_display_names() {
    let sources = Arc::new(
        InMemorySource::new()
            .with_module(
                ModuleId::new("main"),
                "--!strict\n\
                 local dep = require('dep')\n\
                 local value: number = dep.value\n\
                 return value",
            )
            .with_module(
                ModuleId::new("dep"),
                "--!strict\n\
                 local broken: number = 'bad'\n\
                 return { value = 1 }",
            )
            .with_metadata(
                ModuleId::new("dep"),
                SourceMetadata::new("display/dep.luau"),
            ),
    );
    let surface = ruau::surface::Surface::builder()
        .module_source(sources)
        .build()
        .expect("surface validates");

    let graph = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::existing("main"),
            Default::default(),
        )
        .expect("module source graph checks");
    let rendered = graph.diagnostics().render();

    assert!(graph.has_errors(), "{rendered}");
    assert!(rendered.contains("display/dep.luau"), "{rendered}");
    let root = graph
        .checked_module(&ModuleName::from("main"))
        .expect("root checked");
    assert!(!root.has_errors(), "{}", root.diagnostics().render("main"));
    let dep = graph
        .checked_module(&ModuleName::from("dep"))
        .expect("dep checked");
    assert!(dep.has_errors(), "{}", dep.diagnostics().render("dep"));
}

#[test]
fn surface_check_module_graph_reports_resolver_errors() {
    let sources = Arc::new(InMemorySource::new().with_module(
        ModuleId::new("main"),
        "--!strict\nreturn require('missing')",
    ));
    let surface = ruau::surface::Surface::builder()
        .module_source(sources)
        .build()
        .expect("surface validates");

    let graph = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::existing("main"),
            Default::default(),
        )
        .expect("module source graph checks");
    let rendered = graph.diagnostics().render();

    assert!(graph.has_errors(), "{rendered}");
    assert!(
        rendered.contains("module `missing` did not resolve"),
        "{rendered}"
    );
}

#[test]
fn surface_check_module_graph_uses_native_require_exports() {
    struct NativeRequireModule;

    impl ruau::vm::NativeModule for NativeRequireModule {
        fn name(&self) -> &str {
            "native"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("declare native: { answer: () -> number }")
        }

        fn export(&self) -> ruau::vm::ModuleExport {
            ruau::vm::ModuleExport::Require
        }

        fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
            builder.leaf_function(
                "answer",
                ruau::vm::ModuleBinding::library("native"),
                |(): ()| 42.0_f64,
            );
        }
    }

    let sources = Arc::new(InMemorySource::new().with_module(
        ModuleId::new("main"),
        "--!strict\n\
         local native = require('native')\n\
         local answer: number = native.answer()\n\
         return answer",
    ));
    let surface = ruau::surface::Surface::builder()
        .module(std::sync::Arc::new(NativeRequireModule))
        .module_source(sources)
        .build()
        .expect("surface validates");

    let graph = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::existing("main"),
            Default::default(),
        )
        .expect("module source graph checks");

    assert!(!graph.has_errors(), "{}", graph.diagnostics().render());
}

#[test]
fn surface_check_graph_overlays_root_and_anchors_relative_requires() {
    let sources = Arc::new(
        InMemorySource::new()
            .with_module(ModuleId::new("pkg/dep"), "--!strict\nreturn { value = 42 }")
            .with_metadata(
                ModuleId::new("pkg/dep"),
                SourceMetadata::new("display/pkg/dep.luau"),
            ),
    );
    let surface = ruau::surface::Surface::builder()
        .module_source(sources)
        .build()
        .expect("surface validates");
    let source = Source::text(
        ModuleId::new("pkg/main"),
        "--!strict\n\
         local dep = require('./dep')\n\
         local value: number = dep.value\n\
         return value",
    )
    .with_metadata(SourceMetadata::new("display/pkg/main.luau"));

    let graph = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::overlay(&source),
            Default::default(),
        )
        .expect("graph is within default limits");

    assert!(!graph.has_errors(), "{}", graph.diagnostics().render());
    assert_eq!(graph.root(), &ModuleName::from("pkg/main"));
    assert!(
        graph.checked_module(&ModuleName::from("pkg/dep")).is_some(),
        "{graph:?}"
    );
}

#[test]
fn surface_graph_default_accepts_declaration_module_sources() {
    let sources = Arc::new(InMemorySource::new().with_module(
        ModuleId::new("api"),
        "export type Module = { ping: () -> string }\n\
         declare module: Module\n\
         return module\n",
    ));
    let surface = ruau::surface::Surface::builder()
        .module_source(sources)
        .build()
        .expect("surface validates");
    let source = Source::text(
        ModuleId::new("main"),
        "local api = require('api')\nreturn api.ping()",
    );

    let graph = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::overlay(&source),
            Default::default(),
        )
        .expect("declaration module graph checks");

    assert!(!graph.has_errors(), "{}", graph.diagnostics().render());
}

#[test]
fn surface_graph_checks_report_missing_module_source() {
    let surface = ruau::surface::Surface::new();

    let error = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::existing("main"),
            Default::default(),
        )
        .expect_err("existing-module graph requires a source");
    assert_eq!(error, ruau::surface::GraphCheckError::MissingModuleSource);

    let source = Source::text(ModuleId::new("main"), "--!strict\nreturn 1");
    let graph = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::overlay(&source),
            Default::default(),
        )
        .expect("graph is within default limits");
    assert!(!graph.has_errors(), "{}", graph.diagnostics().render());
}

#[test]
fn existing_root_graphs_enforce_default_module_limits() {
    let mut modules = InMemorySource::new();
    let mut root = String::new();
    for index in 0..1_024 {
        let name = format!("dep{index}");
        modules.insert(ModuleId::new(name.clone()), "return 1");
        root.push_str(&format!("require('{name}')\n"));
    }
    root.push_str("return 1");
    modules.insert(ModuleId::new("main"), root);
    let surface = ruau::surface::Surface::builder()
        .module_source(Arc::new(modules))
        .build()
        .expect("surface validates");

    let error = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::existing("main"),
            Default::default(),
        )
        .expect_err("the root-inclusive default module limit is finite");

    assert!(matches!(
        error,
        ruau::surface::GraphCheckError::Limit(ref limit)
            if limit.kind() == ruau::typecheck::GraphLimitKind::Modules
                && limit.maximum() == 1_024
                && limit.observed() == 1_025
    ));
}

#[test]
fn graph_mode_override_applies_only_to_the_root() {
    let modules = Arc::new(
        InMemorySource::new()
            .with_module(ModuleId::new("main"), "return require('dep')")
            .with_module(ModuleId::new("dep"), "return 1"),
    );
    let surface = ruau::surface::Surface::builder()
        .analysis_mode(ruau::typecheck::Mode::Nonstrict)
        .module_source(modules)
        .build()
        .expect("surface validates");
    let graph = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::existing("main"),
            ruau::surface::GraphCheckOptions::default().with_mode(ruau::typecheck::Mode::Strict),
        )
        .expect("graph checks");

    assert_eq!(
        graph
            .checked_module(&ModuleName::from("main"))
            .expect("root is checked")
            .mode(),
        ruau::typecheck::Mode::Strict
    );
    assert_eq!(
        graph
            .checked_module(&ModuleName::from("dep"))
            .expect("dependency is checked")
            .mode(),
        ruau::typecheck::Mode::Nonstrict
    );
}

#[test]
fn surface_check_graph_handles_text_and_byte_roots() {
    let surface = ruau::surface::Surface::new();
    let source = "--!strict\nlocal value: number = 1\nreturn value";

    let text_source = Source::text(ModuleId::new("main"), source);
    let byte_source = Source::bytes(ModuleId::new("main"), source.as_bytes());
    let text = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::overlay(&text_source),
            Default::default(),
        )
        .expect("text graph is within default limits");
    let bytes = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::overlay(&byte_source),
            Default::default(),
        )
        .expect("byte graph is within default limits");

    assert!(!text.has_errors(), "{}", text.diagnostics().render());
    assert!(!bytes.has_errors(), "{}", bytes.diagnostics().render());
    assert_eq!(text.build_queue(), bytes.build_queue());
    assert_eq!(text.checked_modules().len(), bytes.checked_modules().len());
}

#[test]
fn surface_graph_checks_expose_async_forms() {
    let sources =
        Arc::new(InMemorySource::new().with_module(ModuleId::new("main"), "--!strict\nreturn 1"));
    let surface = ruau::surface::Surface::builder()
        .module_source(sources)
        .build()
        .expect("surface validates");

    let module_graph = block_on_test(surface.check_graph(
        ruau::surface::GraphRoot::existing("main"),
        Default::default(),
    ))
    .expect("async module graph checks");
    let source = Source::text(ModuleId::new("main"), "--!strict\nreturn 1");
    let source_graph = block_on_test(surface.check_graph(
        ruau::surface::GraphRoot::overlay(&source),
        Default::default(),
    ))
    .expect("source graph is within default limits");

    assert!(
        !module_graph.has_errors(),
        "{}",
        module_graph.diagnostics().render()
    );
    assert!(
        !source_graph.has_errors(),
        "{}",
        source_graph.diagnostics().render()
    );
}

#[test]
fn downstream_native_modules_expose_an_export_mode() {
    struct DefaultExportModule;
    struct RequireExportModule;

    impl ruau::vm::NativeModule for DefaultExportModule {
        fn name(&self) -> &str {
            "default_export"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text(
                "declare default_export: { ping: () -> number }",
            )
        }

        fn install(&self, _builder: &mut dyn ruau::vm::module::Installer) {}
    }

    impl ruau::vm::NativeModule for RequireExportModule {
        fn name(&self) -> &str {
            "require_export"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text(
                "declare require_export: { ping: () -> number }",
            )
        }

        fn export(&self) -> ruau::vm::ModuleExport {
            ruau::vm::ModuleExport::Require
        }

        fn install(&self, _builder: &mut dyn ruau::vm::module::Installer) {}
    }

    assert_eq!(
        ruau::vm::NativeModule::export(&DefaultExportModule),
        ruau::vm::ModuleExport::Globals
    );
    assert_eq!(
        ruau::vm::NativeModule::export(&RequireExportModule),
        ruau::vm::ModuleExport::Require
    );
    assert_eq!(
        ruau::vm::ModuleExport::default(),
        ruau::vm::ModuleExport::Globals
    );
}

#[test]
fn downstream_host_module_manifest_tracks_export_mode() {
    struct ExportModeModule {
        export: ruau::vm::ModuleExport,
    }

    impl ruau::vm::NativeModule for ExportModeModule {
        fn name(&self) -> &str {
            "mode"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("declare mode: { ping: () -> number }")
        }

        fn export(&self) -> ruau::vm::ModuleExport {
            self.export
        }

        fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
            use ruau::vm::module::InstallerExt;
            builder.leaf_function(
                "ping",
                ruau::vm::ModuleBinding::library("mode"),
                |(): ()| 1.0_f64,
            );
        }
    }

    let globals = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .module(std::sync::Arc::new(ExportModeModule {
            export: ruau::vm::ModuleExport::Globals,
        }))
        .build()
        .expect("global module surface builds");
    let require = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .module(std::sync::Arc::new(ExportModeModule {
            export: ruau::vm::ModuleExport::Require,
        }))
        .build()
        .expect("require module surface builds");

    assert_ne!(
        globals.host_module_manifest_version(),
        require.host_module_manifest_version(),
        "export mode is part of the host-module manifest hash"
    );
}

#[test]
fn downstream_require_export_modules_accept_exported_type_aliases() {
    struct RequireExportAliasModule;

    impl ruau::vm::NativeModule for RequireExportAliasModule {
        fn name(&self) -> &str {
            "native"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text(
                "declare native: { answer: () -> number }\n\
                 export type NativeModule = typeof(native)",
            )
        }

        fn export(&self) -> ruau::vm::ModuleExport {
            ruau::vm::ModuleExport::Require
        }

        fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
            builder.leaf_function(
                "answer",
                ruau::vm::ModuleBinding::library("native"),
                |(): ()| 42.0_f64,
            );
        }
    }

    let surface = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .module(std::sync::Arc::new(RequireExportAliasModule))
        .build()
        .expect("exported aliases on require modules are valid declarations");
    let checked = surface
        .new_checker()
        .check_source("--!strict\nlocal native = require(\"native\")\nreturn native.answer()");
    let summary = checked.diagnostics().render("main.luau");
    assert!(!checked.has_errors(), "{summary}");
}

struct DemoThing;

fn demo_thing_type() -> ruau::vm::HostType {
    HostTypeBuilder::<DemoThing>::new("DemoThing")
        .declaration("declare class DemoThing\nend")
        .build()
}

struct DemoExportModule {
    name: &'static str,
    export: ruau::vm::ModuleExport,
    answer: f64,
}

impl ruau::vm::NativeModule for DemoExportModule {
    fn name(&self) -> &str {
        self.name
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text({
            match self.name {
                "demo_globals" => "declare demo_globals: { answer: () -> number }",
                "demo_require" => "declare demo_require: { answer: () -> number }",
                "demo_both" => {
                    "declare class DemoThing\nend\n\
                 type DemoSource = { value: number }\n\
                 declare demo_both: { answer: () -> number, source: DemoSource }"
                }
                _ => unreachable!("test module names are fixed"),
            }
        })
    }

    fn export(&self) -> ruau::vm::ModuleExport {
        self.export
    }

    fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
        if self.name == "demo_both" {
            InstallerExt::host_type(builder, demo_thing_type());
            builder.support_chunk("demo.support", b"return { answer = 17 }");
            builder.source_value(
                "source",
                ruau::vm::ModuleBinding::library("demo_both"),
                b"return table.freeze({ value = 11 })",
            );
        }
        let answer = self.answer;
        builder.leaf_function(
            "answer",
            ruau::vm::ModuleBinding::library(self.name),
            move |(): ()| answer,
        );
    }
}

fn demo_surface() -> ruau::surface::Surface {
    ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .module(std::sync::Arc::new(DemoExportModule {
            name: "demo_globals",
            export: ruau::vm::ModuleExport::Globals,
            answer: 3.0,
        }))
        .module(std::sync::Arc::new(DemoExportModule {
            name: "demo_require",
            export: ruau::vm::ModuleExport::Require,
            answer: 5.0,
        }))
        .module(std::sync::Arc::new(DemoExportModule {
            name: "demo_both",
            export: ruau::vm::ModuleExport::Both,
            answer: 7.0,
        }))
        .build()
        .expect("demo module surface validates")
}

#[test]
fn downstream_demo_module_exercises_export_modes_and_builder_extras() {
    const SOURCE: &str = r#"--!strict
local required = require("demo_require")
local both = require("demo_both")
local total: number = demo_globals.answer()
    + required.answer()
    + both.answer()
    + demo_both.answer()
    + both.source.value
    + demo_both.source.value
assert(not pcall(function() both.source.value = 12 end))
return total
"#;
    let surface = demo_surface();
    let checked = surface.new_checker().check_source(SOURCE);
    let summary = checked.diagnostics().render("demo.luau");
    assert!(!checked.has_errors(), "{summary}");

    let mut vm = surface
        .vm_builder(&vm_config(0))
        .build()
        .expect("demo VM builds");
    let chunk = compile_bytes(&surface, SOURCE.as_bytes()).expect("demo source compiles");
    let module = vm.load(&chunk).expect("demo chunk loads");
    let values = vm
        .call_protected(&module, Default::default())
        .expect("demo call is not fatal")
        .expect("demo script succeeds");
    assert_eq!(format!("{values:?}"), "[Number(44.0)]");

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

impl ruau::vm::NativeModule for HostEvalModule {
    fn name(&self) -> &str {
        "host"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("declare host: { value: () -> string }")
    }

    fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
        builder.scoped_function(
            "value",
            ruau::vm::ModuleBinding::library("host"),
            Box::new(HostConfigValue),
        );
    }
}

struct CancellationProbeModule {
    cancellation: Arc<Mutex<Option<ruau::vm::Cancel>>>,
    future_dropped: Arc<AtomicBool>,
}

struct HostFutureDropProbe(Arc<AtomicBool>);

impl Drop for HostFutureDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl ruau::vm::NativeModule for CancellationProbeModule {
    fn name(&self) -> &str {
        "cancellation_probe"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("declare cancellation_probe: { wait: () -> () }")
    }

    fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
        let cancellation = Arc::clone(&self.cancellation);
        let future_dropped = Arc::clone(&self.future_dropped);
        builder.async_function_fn(
            "wait",
            ruau::vm::ModuleBinding::library("cancellation_probe"),
            move |ctx, (): ()| {
                let cancellation_slot = Arc::clone(&cancellation);
                let future_dropped = Arc::clone(&future_dropped);
                async move {
                    let cancellation = ctx.cancellation().ok_or_else(|| {
                        ruau::vm::RuntimeError::runtime("missing evaluation cancellation signal")
                    })?;
                    let _drop_probe = HostFutureDropProbe(future_dropped);
                    *cancellation_slot.lock().expect("cancellation lock") = Some(cancellation);
                    pending::<Result<ruau::vm::HostReturn, ruau::vm::RuntimeError>>().await
                }
            },
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

#[test]
fn downstream_evaluator_evaluates_with_args_app_data_and_prints() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());
    let outcome = host
        .eval_blocking(
            "print(\"hello\")\nreturn args.name, host.value()",
            ruau::eval::Options::default()
                .chunk_name("host-success.luau")
                .args(serde_json::json!({ "name": "Ada" }))
                .app_data(HostConfig("app-data".to_owned())),
        )
        .expect("eval succeeds");

    assert_eq!(outcome.prints, ["hello"]);
    assert_eq!(outcome.value, Some(serde_json::json!(["Ada", "app-data"])));
}

#[test]
fn downstream_checked_evaluator_matches_successful_unchecked_execution() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());
    let source = "--!strict\nprint(\"hello\")\nreturn args.name, host.value()";

    let checked = host
        .eval_checked_blocking(
            source,
            ruau::eval::Options::default()
                .chunk_name("host-checked-success.luau")
                .args(serde_json::json!({ "name": "Ada" }))
                .app_data(HostConfig("app-data".to_owned())),
        )
        .expect("checked eval succeeds");
    let unchecked = host
        .eval_blocking(
            source,
            ruau::eval::Options::default()
                .chunk_name("host-unchecked-success.luau")
                .args(serde_json::json!({ "name": "Ada" }))
                .app_data(HostConfig("app-data".to_owned())),
        )
        .expect("unchecked eval succeeds");

    assert_eq!(checked.prints, ["hello"]);
    assert_eq!(checked.value, Some(serde_json::json!(["Ada", "app-data"])));
    assert_eq!(checked.prints, unchecked.prints);
    assert_eq!(checked.value, unchecked.value);
    assert!(
        checked.timing.check.is_some(),
        "checked path records check timing"
    );
    assert!(
        unchecked.timing.check.is_none(),
        "unchecked path skips check timing"
    );
}

#[test]
fn downstream_checked_evaluator_reports_static_errors_without_changing_unchecked_eval() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());
    let source = "--!strict\nlocal value: number = \"bad\"\nreturn value";

    let error = host
        .eval_checked_blocking(
            source,
            ruau::eval::Options::default().chunk_name("checked-type-error.luau"),
        )
        .expect_err("checked eval rejects type diagnostics");

    assert_eq!(error.kind, ruau::eval::ErrorKind::Check);
    assert_eq!(error.chunk_name(), "checked-type-error.luau");
    assert!(error.line.is_some(), "{error:?}");
    assert!(error.message.contains("checked-type-error.luau"));
    assert!(error.format_pretty().contains("^"));

    let unchecked = host
        .eval_blocking(
            source,
            ruau::eval::Options::default().chunk_name("unchecked-type-error.luau"),
        )
        .expect("unchecked eval still compiles and runs");
    assert_eq!(unchecked.value, Some(serde_json::json!("bad")));
}

#[test]
fn downstream_checked_evaluator_reports_module_source_graph_errors() {
    let source = Arc::new(
        InMemorySource::new()
            .with_module(
                ModuleId::new("app/dep"),
                "--!strict\nlocal value: number = \"bad\"\nreturn value",
            )
            .with_metadata(
                ModuleId::new("app/dep"),
                SourceMetadata::new("display/app/dep.luau"),
            ),
    );
    let surface = ruau::surface::Surface::builder()
        .module_source(source)
        .build()
        .expect("surface validates");
    let host = ruau::eval::Evaluator::new(surface);

    let error = host
        .eval_checked_blocking(
            "--!strict\nreturn require('./dep')",
            ruau::eval::Options::default().chunk_name("app/main"),
        )
        .expect_err("dependency type diagnostic rejects checked eval");

    assert_eq!(error.kind, ruau::eval::ErrorKind::Check);
    assert!(error.message.contains("display/app/dep.luau"), "{error:?}");
    assert_eq!(
        error.line, None,
        "dependency diagnostics should not point at the root source excerpt"
    );
}

#[test]
fn downstream_surface_session_runs_raw_retained_values() {
    let surface = host_eval_surface();
    let session =
        ruau::session::SharedRuntime::new(surface.clone(), &vm_config(0)).expect("session builds");
    let chunk = compile_bytes(&surface, b"return 7, 'raw'").expect("source compiles");

    let outcome = session
        .run_compiled_blocking(
            &chunk,
            &ruau::session::LoadTarget::named("session-public.luau"),
            ruau::vm::CallOptions::new(),
        )
        .expect("session run succeeds");

    assert!(matches!(
        outcome.values.as_slice(),
        [
            ruau::vm::ValueSnapshot::Integer(7) | ruau::vm::ValueSnapshot::Number(7.0),
            ruau::vm::ValueSnapshot::String(value)
        ] if value.as_slice() == b"raw"
    ));
    assert!(outcome.execution_count > 0);
}

#[test]
fn downstream_evaluator_reports_compile_errors_with_source_context() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());
    let error = host
        .eval_blocking(
            "local =",
            ruau::eval::Options::default().chunk_name("bad.luau"),
        )
        .expect_err("compile fails");

    assert_eq!(error.kind, ruau::eval::ErrorKind::Compile);
    assert!(error.line.is_some(), "{error:?}");
    assert!(error.format_pretty().contains("^"));
}

#[test]
fn downstream_evaluator_defaults_to_bounded_untrusted_execution() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());
    let error = host
        .eval_blocking(
            "while true do end",
            ruau::eval::Options::default().chunk_name("default-timeout.luau"),
        )
        .expect_err("default options terminate a busy script");

    // The default posture is doubly bounded: whichever of the wall clock or
    // the gas budget trips first terminates the script.
    assert!(
        matches!(
            error.kind,
            ruau::eval::ErrorKind::Timeout | ruau::eval::ErrorKind::Runtime
        ),
        "unexpected kind: {error:?}"
    );
    assert!(
        error.message.contains("timed out") || error.message.contains("budget"),
        "unexpected message: {error:?}"
    );
}

#[test]
fn downstream_evaluator_times_out_busy_scripts() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());
    let error = host
        .eval_blocking(
            "while true do end",
            ruau::eval::Options::default()
                .chunk_name("timeout.luau")
                .timeout(Duration::from_millis(20))
                .limits(ruau::vm::Limits::unlimited()),
        )
        .expect_err("busy script times out");

    assert_eq!(error.kind, ruau::eval::ErrorKind::Timeout);
}

#[test]
fn downstream_checked_evaluator_times_out_after_static_checking() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());
    let error = host
        .eval_checked_blocking(
            "--!strict\nwhile true do end",
            ruau::eval::Options::default()
                .chunk_name("checked-timeout.luau")
                .timeout(Duration::from_millis(20))
                .limits(ruau::vm::Limits::unlimited()),
        )
        .expect_err("checked busy script times out after checking");

    assert_eq!(error.kind, ruau::eval::ErrorKind::Timeout);
}

#[test]
fn downstream_checked_evaluator_honors_external_cancellation_after_static_checking() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());
    let cancel = ruau::vm::Cancel::manual();
    let trigger = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        trigger.cancel();
    });

    let error = host
        .eval_checked_blocking(
            "--!strict\nwhile true do end",
            ruau::eval::Options::trusted()
                .chunk_name("checked-cancelled.luau")
                .cancel(cancel),
        )
        .expect_err("external cancellation stops checked execution");

    canceller.join().expect("canceller thread exits");
    assert_eq!(error.kind, ruau::eval::ErrorKind::Cancelled);
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_checked_evaluator_exposes_deadline_and_drops_host_future() {
    let cancellation = Arc::new(Mutex::new(None));
    let future_dropped = Arc::new(AtomicBool::new(false));
    let surface = ruau::surface::Surface::builder()
        .module(Arc::new(CancellationProbeModule {
            cancellation: Arc::clone(&cancellation),
            future_dropped: Arc::clone(&future_dropped),
        }))
        .build()
        .expect("cancellation probe surface validates");
    let evaluator = ruau::eval::Evaluator::new(surface);

    let error = evaluator
        .eval_checked(
            "--!strict\ncancellation_probe.wait()\nreturn 1",
            ruau::eval::Options::default()
                .chunk_name("checked-host-deadline.luau")
                .timeout(Duration::from_millis(20))
                .limits(ruau::vm::Limits::unlimited()),
        )
        .await
        .expect_err("deadline stops the checked async host call");

    assert_eq!(error.kind, ruau::eval::ErrorKind::Timeout);
    let stop_reason = cancellation
        .lock()
        .expect("cancellation lock")
        .as_ref()
        .and_then(ruau::vm::Cancel::stop_reason);
    assert_eq!(
        stop_reason,
        Some(ruau::vm::StopReason::Deadline),
        "the async host sees the evaluator deadline cause"
    );
    assert!(
        future_dropped.load(Ordering::SeqCst),
        "the host future drops before eval_checked returns its terminal error"
    );
}

#[test]
fn downstream_evaluator_trusted_options_disable_default_timeout() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());
    let cancel = ruau::vm::Cancel::manual();
    let trigger = cancel.clone();
    let started = Instant::now();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(ruau::eval::DEFAULT_TIMEOUT + Duration::from_millis(25));
        trigger.cancel();
    });

    let error = host
        .eval_blocking(
            "while true do end",
            ruau::eval::Options::trusted()
                .chunk_name("trusted-timeout.luau")
                .cancel(cancel),
        )
        .expect_err("external cancellation stops the trusted unbounded run");

    canceller.join().expect("canceller thread exits");
    assert_eq!(error.kind, ruau::eval::ErrorKind::Cancelled);
    assert!(
        started.elapsed() >= ruau::eval::DEFAULT_TIMEOUT,
        "trusted options should not install the default timeout: {error:?}"
    );
}

#[test]
fn downstream_evaluator_times_out_many_calls_on_shared_timer() {
    let host = ruau::eval::Evaluator::new(host_eval_surface());

    for index in 0..32 {
        let error = host
            .eval_blocking(
                "while true do end",
                ruau::eval::Options::default()
                    .chunk_name(format!("batch-timeout-{index}.luau"))
                    .timeout(Duration::from_millis(5))
                    .limits(ruau::vm::Limits::unlimited()),
            )
            .expect_err("each busy script times out");

        assert_eq!(error.kind, ruau::eval::ErrorKind::Timeout);
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
        .vm_builder(&vm_config(0))
        .build()
        .expect("VM builds without installing declaration-only globals");
    let chunk = compile_bytes(&surface, b"return args == nil").expect("compiles");
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

    impl ruau::vm::NativeModule for AcmeModule {
        fn name(&self) -> &str {
            "acme"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text({
                "type Verdict = number\n\
             declare acme: { answer: () -> Verdict }"
            })
        }

        fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
            use ruau::vm::module::InstallerExt;
            builder.scoped_function(
                "answer",
                ruau::vm::ModuleBinding::library("acme"),
                ruau::vm::scoped_host_fn(answer),
            );
        }
    }

    let surface = ruau::surface::Surface::builder()
        .libraries([])
        .module(std::sync::Arc::new(AcmeModule))
        .require_global("decide", "(Verdict) -> (Verdict?, string?)")
        .build()
        .expect("surface validates");

    let strict_config = || ruau::typecheck::Config::with_source_mode(ruau::typecheck::Mode::Strict);

    // A conforming definition passes (and may use the declared module).
    let mut checker = surface.new_checker();
    let checked = checker.check_source_with_config(
        "function decide(v: number): (number?, string?)\n\
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
    let checked = checker.check_source_with_config("local x = 1", strict_config());
    let required: Vec<_> = checked
        .diagnostics()
        .iter()
        .filter(|diagnostic| {
            diagnostic.category == ruau::typecheck::DiagnosticCategory::RequiredExport
        })
        .collect();
    assert_eq!(required.len(), 1, "{:?}", checked.diagnostics());
    assert_eq!(required[0].code(), 1012);
    assert_eq!(
        *required[0].payload(),
        serde_json::json!({
            "kind": "required-export",
            "name": "decide",
            "required": "(Verdict) -> (Verdict?, string?)",
        })
    );
    let required_view = checked
        .diagnostics()
        .views()
        .find(|diagnostic| {
            diagnostic.category == &ruau::typecheck::DiagnosticCategory::RequiredExport
        })
        .expect("required-export view is present");
    assert_eq!(required_view.severity, ruau::typecheck::Severity::Error);
    assert_eq!(required_view.code, 1012);
    assert!(required_view.primary_location.is_missing());
    assert_eq!(
        required_view.message,
        "Required global 'decide' is not defined; expected '(Verdict) -> (Verdict?, string?)'"
    );
    assert!(matches!(
        required_view.payload,
        ruau::typecheck::Payload::RequiredExport {
            name,
            required,
            actual: None,
        } if name == "decide" && required == "(Verdict) -> (Verdict?, string?)"
    ));

    // A mismatched definition reports the rendered actual type.
    let mut checker = surface.new_checker();
    let checked = checker.check_source_with_config(
        "function decide(v: string): (number?, string?)\n\
          \treturn nil, v\n\
          end",
        strict_config(),
    );
    assert!(
        checked.diagnostics().iter().any(|diagnostic| {
            diagnostic.category == ruau::typecheck::DiagnosticCategory::RequiredExport
                && matches!(
                    &diagnostic.typed_payload,
                    ruau::typecheck::Payload::RequiredExport {
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
    let mut mutable_surface = surface;
    let error = mutable_surface
        .require_global("decide", "(Unknowable) -> number")
        .expect_err("undeclared type names are rejected");
    match error {
        ruau::surface::ConfigError::InvalidRequiredGlobal { name, .. } => {
            assert_eq!(name, "decide");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let error = ruau::surface::Surface::builder()
        .require_global("decide", "(Unknowable) -> number")
        .build()
        .expect_err("builder-time required globals are validated");
    match error {
        ruau::surface::ConfigError::InvalidRequiredGlobal { name, .. } => {
            assert_eq!(name, "decide");
        }
        other => panic!("unexpected builder error: {other:?}"),
    }
}

#[test]
fn downstream_surfaces_accept_declaration_only_modules() {
    let surface = ruau::surface::Surface::builder()
        .declaration_module(
            "hotki-api",
            ruau_declaration::DeclarationSource::Text(
                "type HotkiItem = { name: string }\n\
                 declare hotki: { select: (HotkiItem) -> () }",
            ),
        )
        .build()
        .expect("declaration-only module validates");

    assert!(surface.native_modules().is_empty());
    assert_eq!(surface.declaration_modules().len(), 1);

    let checked = surface.check(
        &Source::text(
            ModuleId::canonicalized("hotki-user"),
            "local item: HotkiItem = { name = \"Ada\" }\nhotki.select(item)",
        ),
        ruau::surface::CheckOptions::default().with_mode(ruau::typecheck::Mode::Strict),
    );
    assert!(
        !checked.has_errors(),
        "{}",
        checked.diagnostics().render("hotki-user.luau")
    );

    let generic_surface = ruau::surface::Surface::builder()
        .declaration_module(
            "generic-api",
            ruau_declaration::DeclarationSource::Text(
                "type Box<T> = { value: T }\n\
                 declare boxes: { string_box: () -> Box<string> }",
            ),
        )
        .build()
        .expect("generic declaration aliases validate through their concrete API uses");
    let checked = generic_surface.check(
        &Source::text(
            ModuleId::canonicalized("generic-user"),
            "local value: string = boxes.string_box().value",
        ),
        ruau::surface::CheckOptions::default().with_mode(ruau::typecheck::Mode::Strict),
    );
    assert!(
        !checked.has_errors(),
        "{}",
        checked.diagnostics().render("generic-user.luau")
    );

    let error = ruau::surface::Surface::builder()
        .declaration_module(
            "bad-types",
            ruau_declaration::DeclarationSource::Text("type Broken = MissingAlias"),
        )
        .build()
        .expect_err("unresolved declaration aliases are rejected");
    match error {
        ruau::surface::ConfigError::InvalidDeclarationModule { module, .. } => {
            assert_eq!(module, "bad-types");
        }
        other => panic!("unexpected declaration-module error: {other:?}"),
    }
}

#[test]
fn downstream_graphs_keep_declaration_module_types_visible() {
    let modules = Arc::new(ruau::source::InMemorySource::new().with_module(
        ruau::source::ModuleId::new("config"),
        "local item: HotkiItem = { name = 'Ada' }\n\
             local boxed: Box<string> = { value = item.name }\n\
             local host_box: Box<string> = boxes.make()\n\
             boxed.value = host_box.value\n\
             return boxed.value",
    ));
    let surface = ruau::surface::Surface::builder()
        .declaration_module(
            "hotki-api",
            ruau_declaration::DeclarationSource::Text(
                "type HotkiItem = { name: string }\n\
                 type Box<T> = { value: T }\n\
                 declare boxes: { make: () -> Box<string> }",
            ),
        )
        .module_source(modules)
        .build()
        .expect("declaration-backed graph surface validates");

    let checked = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::existing("config"),
            Default::default(),
        )
        .expect("surface has a module source");
    assert!(!checked.has_errors(), "{}", checked.diagnostics().render());
}

#[test]
fn downstream_surfaces_check_bytes_with_explicit_mode_without_prefixing_source() {
    let surface = ruau::surface::Surface::builder()
        .analysis_mode(ruau::typecheck::Mode::Nonstrict)
        .build()
        .expect("surface validates");

    let source = b"local x: { foo: string }? = nil\nlocal y = x.foo\nlocal _ = y";
    let checked = surface.check(
        &Source::bytes(ModuleId::canonicalized("strict-bytes"), source.to_vec()),
        ruau::surface::CheckOptions::default().with_mode(ruau::typecheck::Mode::Strict),
    );
    assert_eq!(checked.mode(), ruau::typecheck::Mode::Strict);
    let diagnostic = checked
        .diagnostics()
        .iter()
        .find(|diagnostic| diagnostic.category == ruau::typecheck::DiagnosticCategory::TypeMismatch)
        .expect("strict mode reports the nil property read");
    assert_eq!(diagnostic.primary_location.begin.line, 1);

    let checked = surface.check(
        &Source::bytes(ModuleId::canonicalized("default-bytes"), source.to_vec()),
        ruau::surface::CheckOptions::default(),
    );
    assert_eq!(checked.mode(), ruau::typecheck::Mode::Nonstrict);
    assert!(!checked.has_errors(), "surface default remains nonstrict");
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_users_can_run_executor_with_filesystem_module_source() {
    let root = temp_root("filesystem-source");
    write_file(&root.join("modules/dep.luau"), "return 37");
    let executor = filesystem_source_executor(&root);

    let outcome = executor
        .run(ruau::executor::Request::new(
            ruau::executor::TenantId(0),
            br#"return require("modules/dep") + 5"#,
            ruau::executor::RunControl::with_timeout(std::time::Duration::from_secs(5))
                .expect("future deadline"),
        ))
        .await
        .expect("filesystem-backed request succeeds");

    assert_eq!(
        outcome.values.as_slice(),
        &[ruau::vm::ValueSnapshot::Number(42.0)]
    );
    assert!(executor.report_metadata().module_source_granted);
    remove_dir(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_filesystem_module_source_rejects_root_escape_requires() {
    let root = temp_root("filesystem-source-escape");
    let outside_stem = format!("{}-outside", root.file_name().unwrap().to_string_lossy());
    let outside = root.with_file_name(format!("{outside_stem}.luau"));
    write_file(&outside, "return 99");
    let executor = filesystem_source_executor(&root);
    let source = format!(r#"return require("modules/../../{outside_stem}")"#);

    let report = executor
        .run_report(ruau::executor::Request::new(
            ruau::executor::TenantId(0),
            source.as_bytes(),
            ruau::executor::RunControl::with_timeout(std::time::Duration::from_secs(5))
                .expect("future deadline"),
        ))
        .await;
    match report.result {
        Err(ruau::executor::RequestError::TypeErrors(diagnostics)) => {
            let has_escape_diagnostic = diagnostics.iter().any(|diagnostic| {
                diagnostic.category == ruau::typecheck::DiagnosticCategory::Resolver
                    && diagnostic
                        .payload()
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
    let executor = filesystem_source_executor(&root);

    let report = executor
        .run_report(ruau::executor::Request::new(
            ruau::executor::TenantId(0),
            br#"return require("bad")"#,
            ruau::executor::RunControl::with_timeout(std::time::Duration::from_secs(5))
                .expect("future deadline"),
        ))
        .await;
    match report.result {
        Err(ruau::executor::RequestError::TypeErrors(diagnostics)) => {
            let diagnostic = diagnostics
                .iter()
                .find(|diagnostic| {
                    diagnostic.category == ruau::typecheck::DiagnosticCategory::Resolver
                })
                .expect("resolver diagnostic is present");
            assert_eq!(
                diagnostic
                    .payload()
                    .get("displayName")
                    .and_then(serde_json::Value::as_str),
                Some("bad.luau")
            );
            let detail = diagnostic
                .payload()
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

#[test]
fn downstream_filesystem_config_reuses_the_validated_source_root() {
    let root = temp_root("filesystem-config-root");
    let direct = ruau::typecheck::config::FilesystemResolver::new(&root)
        .expect("direct config root validates");
    let directory =
        ruau::source::fs::Directory::new(&root).expect("filesystem source root validates");
    let shared = directory.config_resolver();

    assert_eq!(direct.root(), root);
    assert_eq!(shared.root(), root);

    remove_dir(&root);
}

/// The umbrella-closure guard (API plan Stage 3.9): every type appearing in
/// an exported item's signature must be nameable through `ruau::` paths. The
/// cheap spelling is binding each signature type to an annotated local —
/// these fail to compile (not at runtime) if a path closes over a private or
/// unexported type.
#[test]
fn umbrella_signature_types_are_nameable() {
    fn assert_debug<T: std::fmt::Debug>() {}
    fn _assert_into_host_return<T: ruau::vm::IntoHostReturn>() {}
    type ExecWithUnitContext = fn(
        &mut ruau::vm::Vm,
        &ruau::vm::LoadedModule,
        &mut (),
        ruau::vm::CallOptions,
    ) -> Result<Vec<ruau::vm::ValueSnapshot>, ruau::vm::ExecError>;

    assert_debug::<ruau::vm::CallOptions>();
    assert_debug::<ruau::vm::Vm>();
    assert_debug::<ruau::session::Runtime>();
    assert_debug::<ruau::session::SharedRuntime>();
    assert_debug::<ruau::eval::Evaluator>();
    assert_debug::<ruau::executor::Executor>();

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
    let _module_array: Option<ruau::vm::module::Array> = None;
    let _kind: Option<ruau::vm::RuntimeErrorKind> = None;
    let _require_kind = ruau::vm::RuntimeErrorKind::UnresolvedRequire;
    let _execution_count: fn(&ruau::vm::Vm) -> u64 = ruau::vm::Vm::execution_count;
    let _default_limits: fn(&ruau::vm::Vm) -> &ruau::vm::Limits = ruau::vm::Vm::default_limits;
    let _set_default_limits: fn(&mut ruau::vm::Vm, ruau::vm::Limits) =
        ruau::vm::Vm::set_default_limits;
    let _exec_with_context: ExecWithUnitContext = ruau::vm::Vm::exec_with_context::<()>;
    // source: module-source family + resolver config.
    let _source: Option<std::sync::Arc<dyn ruau::source::SourceProvider>> = None;
    let _sync_source: Option<Box<dyn ruau::source::SyncSourceProvider>> = None;
    let _resolver: Option<Box<dyn ruau::typecheck::config::Resolver>> = None;
    let _read_request: Option<ruau::source::ReadContext<'static>> = None;
    let _instance_key: Option<ruau::source::InstanceKey> = None;
    let _mount_epoch: Option<ruau::source::SourceEpoch> = None;
    let _meta: Option<ruau::source::SourceMetadata> = None;
    let _result: Option<ruau::source::SourceResult<Vec<u8>>> = None;
    let _error: Option<ruau::source::SourceError> = None;
    let _resolved_from = ruau::source::resolve_request_from(
        &ruau::source::ModuleId::new("root/main"),
        b"./dep".as_slice(),
    );
    let _mode2: Option<ruau::typecheck::Mode> = None;
    let _surface_builder = ruau::surface::Surface::builder()
        .enable_runtime_compilation()
        .analysis_mode(ruau::typecheck::Mode::Nonstrict)
        .declaration_module(
            "api-shape",
            ruau_declaration::DeclarationSource::Text("type Shape = string"),
        );
    let _vm_config: Option<ruau::surface::VmConfig> = None;
    let _config_error = ruau::surface::ConfigError::InvalidDeclarationModule {
        module: String::new(),
        reason: String::new(),
    };
    let _check: fn(
        &ruau::surface::Surface,
        &ruau::source::Source,
        ruau::surface::CheckOptions,
    ) -> ruau::typecheck::CheckedModule = ruau::surface::Surface::check;
    let _prepare_options: Option<ruau::surface::PrepareOptions> = None;
    let _prepare_error: Option<ruau::surface::PrepareError> = None;
    let _prepared_script: Option<ruau::surface::PreparedSource> = None;
    let _prepared_run_error: Option<ruau::surface::PreparedRunError> = None;
    let _aliased_source = ruau::source::InMemorySource::new()
        .with_alias(ModuleId::new("@core/dep"), ModuleId::new("dep"));
    // compile: the safe entry's full signature closure.
    let _chunk: Option<ruau::bytecode::BytecodeChunk> = None;
    let _cerr: Option<ruau::bytecode::CompileError> = None;
    let _ckind: Option<ruau::bytecode::CompileErrorKind> = None;
    let _cloc: Option<ruau::syntax::Location> = None;
    let _byte_offset = ruau::syntax::Position::new(0, 0).byte_offset("");
    let _byte_range = ruau::syntax::Location::new(
        ruau::syntax::Position::new(0, 0),
        ruau::syntax::Position::new(0, 0),
    )
    .byte_range("");
    let _opts: Option<ruau::bytecode::CompileOptions> = None;
    // types: checking + schema strata.
    let _schema: Option<ruau::typecheck::schema::Module> = None;
    let _tschema: Option<ruau::typecheck::schema::Type> = None;
    let _sdiag: Option<ruau::typecheck::ModuleDiagnostic> = None;
    let _conformance: Option<ruau::typecheck::ConformanceCheck> = None;
    let _conformance_fingerprint: Option<ruau::typecheck::ConformanceFingerprint> = None;
    // diagnostic: the checker's reporting closure.
    let _diag: Option<ruau::typecheck::Diagnostic> = None;
    let _loc: Option<ruau::typecheck::DiagnosticLocation> = None;
    let _sev: Option<ruau::typecheck::Severity> = None;
    // embed: marshaled values (also `durable`'s state snapshot type).
    let _mv: Option<ruau::vm::ValueSnapshot> = None;
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
    let _raw: Option<ruau_vm::RawValue> = None;
    let _raw_unwind: Option<ruau_vm::Unwind> = None;
    let _unwind: Option<ruau::vm::HostUnwind> = None;
    let _host_return: Option<ruau::vm::HostReturn> = None;
    let _script_error_field: Option<ruau::vm::ScriptErrorField> = None;
    let _str: Option<ruau::vm::Str<'static>> = None;
    let _stashed_table: Option<ruau::vm::StashedTable> = None;
    _assert_into_host_return::<ruau::vm::StashedTable>();
    let _table_id: Option<ruau::vm::TableId> = None;
    let _invocation_usage: Option<ruau::session::InvocationPollUsage> = None;
    let _invocation_step: Option<ruau::session::InvocationStep<(), ()>> = None;
    let _invocation_error: Option<ruau::session::InvocationError<()>> = None;
    let _key: Option<ruau::vm::KeyHandle> = None;
    let _pek: Option<ruau::syntax::parse::ErrorKind> = None;
    let _comment: Option<ruau::syntax::parse::Comment> = None;
    let _hot: Option<ruau::syntax::parse::HotComment> = None;
    let _ck: Option<ruau::syntax::parse::CommentKind> = None;
    let _acfg: Option<ruau::typecheck::config::ModuleConfig> = None;
    let _gcfg: Option<ruau::typecheck::GenerationConfig> = None;
    let _payload: Option<ruau::typecheck::Payload> = None;
    let _json: Option<serde_json::Value> = None;
    let _fsres: Option<ruau::typecheck::config::FilesystemResolver> = None;
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
        .trusted_host()
        .build()
        .expect("vm builds");
    let chunk = ruau::vm::RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(b"return 0", &ruau::bytecode::CompileOptions::default())
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

    impl ruau::vm::NativeModule for RuntimeNamedModule {
        fn name(&self) -> &str {
            &self.name
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text(&self.declaration)
        }

        fn install(&self, builder: &mut dyn ruau::vm::module::Installer) {
            use ruau::vm::module::InstallerExt;
            builder.scoped_function(
                &self.member,
                ruau::vm::ModuleBinding::library(self.library.clone()),
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
        .vm_builder(&ruau::surface::VmConfig::untrusted(
            ruau::vm::Ambient::production(0),
            ruau::vm::Limits::metered(100_000, 1 << 20),
        ))
        .build()
        .expect("vm builds");
    let chunk = compile_bytes(
        &surface,
        format!("assert({owner}.{member}() == 42, \"wrong answer\") return 0").as_bytes(),
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
/// into scope-borrowed Lua values and back, and owned `ValueSnapshot` trees
/// convert to and from `serde_json::Value` — all nameable from the umbrella
/// crate.
#[test]
fn serde_bridge_values_through_the_public_path() {
    use ruau::vm::{
        ScopedValue, ValueSnapshot,
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
        .trusted_host()
        .build()
        .expect("vm builds");
    let chunk = ruau::vm::RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(
            b"return { kind = 'go', dx = 2, dy = 3 }",
            &ruau::bytecode::CompileOptions::default(),
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
    let opaque = ValueSnapshot::Opaque("function");
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
        .trusted_host()
        .build()
        .expect("vm builds");
    let chunk = ruau::vm::RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(b"base = 40", &ruau::bytecode::CompileOptions::default())
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
        .trusted_host()
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
    use ruau::vm::{ScopedValue, ValueSnapshot};

    let mut vm = ruau::vm::Vm::builder()
        .ambient(ruau::vm::Ambient::deterministic(0))
        .limits(ruau::vm::Limits::unlimited())
        .runtime_capabilities(ruau::vm::RuntimeCapabilities::default().enable_runtime_compilation())
        .trusted_host()
        .build()
        .expect("vm builds");
    let chunk = ruau::vm::RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(
            b"return { answer = 42, bytes = buffer.fromstring('bytes') }",
            &ruau::bytecode::CompileOptions::default(),
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

    let ValueSnapshot::Table(pairs) = snapshot else {
        panic!("expected table snapshot, got {snapshot:?}");
    };
    assert!(
        pairs.iter().any(|pair| {
            matches!(
                (&pair.key, &pair.value),
                (ValueSnapshot::String(key), ValueSnapshot::Number(value))
                    if key == b"answer" && (*value - 42.0).abs() < f64::EPSILON
            )
        }),
        "{pairs:?}"
    );
    assert!(
        pairs.iter().any(|pair| {
            matches!(
                (&pair.key, &pair.value),
                (ValueSnapshot::String(key), ValueSnapshot::Buffer(value))
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
    use ruau::vm::{
        Ambient, CompiledModule, Library, Limits, LoadError, RuntimeCapabilities, Vm, VmBuildError,
    };

    let runtime_capabilities = RuntimeCapabilities::from_libraries(libraries_except(Library::Os));
    let surface = ruau::surface::Surface::builder()
        .libraries(libraries_except(Library::Os))
        .build()
        .expect("surface builds");
    let source = Source::text(
        ModuleId::new("artifact/main.luau"),
        "assert(6 * 7 == 42, \"wrong answer\") return 0",
    );
    let artifact: CompiledModule = surface
        .compile_module(&source, &ruau::bytecode::CompileOptions::default())
        .expect("surface compiles the artifact");
    assert_eq!(artifact.runtime_capabilities(), &runtime_capabilities);
    let artifact_with_options = surface
        .compile_module(&source, &ruau::bytecode::CompileOptions::default())
        .expect("surface compiles source artifacts with options");
    assert_eq!(
        artifact_with_options.runtime_capabilities(),
        &runtime_capabilities
    );
    let bytes_artifact = surface
        .compile_module(
            &Source::bytes(
                ModuleId::canonicalized("artifact/bytes.luau"),
                b"assert(6 * 7 == 42, \"wrong answer\") return 0".to_vec(),
            ),
            &ruau::bytecode::CompileOptions::default(),
        )
        .expect("surface compiles byte artifacts with options");
    assert_eq!(bytes_artifact.runtime_capabilities(), &runtime_capabilities);

    // Build-time instantiation through the surface-aligned builder.
    let mut vm = surface
        .vm_builder(&vm_config(0))
        .preload(&artifact)
        .build()
        .expect("preloaded vm builds");
    let modules = vm.take_preloaded();
    assert_eq!(modules.len(), 1);
    vm.call(&modules[0], Default::default())
        .expect("preloaded module runs");

    // Post-build instantiation of the same artifact into a second VM.
    let mut second = surface
        .vm_builder(&vm_config(1))
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
        .trusted_host()
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
            .trusted_host()
            .build(),
        Err(VmBuildError::Preload(
            LoadError::RuntimeCapabilitiesMismatch { .. }
        ))
    ));
}
