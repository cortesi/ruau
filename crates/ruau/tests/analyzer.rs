//! Integrated analyzer API tests.
#![allow(clippy::tests_outside_test_module)]

use std::{env, fs, path::PathBuf, process, time::Duration};

use ruau::{
    HostApi, Luau,
    analyzer::{
        AnalysisError, CancellationToken, CheckOptions, Checker, Severity, VirtualModule,
        extract_entrypoint_schema,
    },
    resolver::{InMemoryResolver, ModuleId, ResolverSnapshot},
};
use tokio::task::yield_now;

fn fixture(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/analyzer")
        .join(path)
}

#[test]
fn strict_type_mismatch_reports_error_without_hot_comment() {
    let mut checker = Checker::new().expect("checker");
    let result = checker
        .check(
            r#"
            local x: number = "hello"
            "#,
        )
        .expect("check");

    assert!(!result.is_ok());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error)
    );
}

#[test]
fn definitions_change_check_behavior() {
    let mut checker = Checker::new().expect("checker");
    checker
        .add_definitions(
            r#"
            declare class TodoBuilder
                function content(self, content: string): TodoBuilder
                function save(self): Todo
            end

            declare class Todo
                function complete(self)
            end

            declare Todo: { create: () -> TodoBuilder }
            "#,
        )
        .expect("definitions");

    let ok = checker
        .check(
            r#"
            local todo = Todo.create():content("Review"):save()
            todo:complete()
            "#,
        )
        .expect("check");
    assert!(ok.is_ok(), "{ok:#?}");

    let bad = checker
        .check(
            r#"
            local _todo = Todo.create():content(42):save()
            "#,
        )
        .expect("check");
    assert!(!bad.is_ok());
}

#[test]
fn invalid_definitions_include_custom_label() {
    let mut checker = Checker::new().expect("checker");
    let error = checker
        .add_definitions_with_name("declare function bad(: string)", "defs/bad.d.luau")
        .expect_err("invalid definitions");

    assert!(error.to_string().contains("defs/bad.d.luau"));
}

#[test]
fn timeout_and_cancellation_are_reported() {
    let mut checker = Checker::new().expect("checker");
    let timeout = checker
        .check_with_options(
            "local x = 1",
            CheckOptions {
                timeout: Some(Duration::ZERO),
                module_name: Some("timeout.luau"),
                ..CheckOptions::default()
            },
        )
        .expect("check");
    assert!(timeout.timed_out);
    assert!(!timeout.is_ok());

    let token = CancellationToken::new().expect("token");
    token.cancel();
    let cancelled = checker
        .check_with_options(
            "local x = 1",
            CheckOptions {
                cancellation_token: Some(&token),
                module_name: Some("cancelled.luau"),
                ..CheckOptions::default()
            },
        )
        .expect("check");
    assert!(cancelled.cancelled);
    assert!(!cancelled.is_ok());
}

#[test]
fn virtual_and_filesystem_modules_resolve() {
    let mut checker = Checker::new().expect("checker");
    let term = VirtualModule {
        name: "term",
        source: r#"
            local module = {}
            function module.current()
                return { cols = 80 }
            end
            return module
        "#,
    };

    let virtual_result = checker
        .check_with_options(
            r#"
            local term = require("term")
            local _: number = term.current().cols
            "#,
            CheckOptions {
                module_name: Some("virtual_root.luau"),
                virtual_modules: &[term],
                ..CheckOptions::default()
            },
        )
        .expect("virtual check");
    assert!(virtual_result.is_ok(), "{virtual_result:#?}");

    let root = fixture("modules/filesystem/requirer.luau");
    let filesystem_result = checker.check_path(&root).expect("filesystem check");
    assert!(filesystem_result.is_ok(), "{filesystem_result:#?}");
}

#[test]
fn diagnostics_include_module_identity() {
    let mut checker = Checker::new().expect("checker");
    let path = env::temp_dir().join(format!("ruau-bad-source-{}.luau", process::id()));
    fs::write(&path, "local value: number = 'wrong'\n").expect("write");

    let result = checker.check_path(&path).expect("check");

    fs::remove_file(&path).expect("remove");
    assert!(!result.is_ok(), "{result:#?}");
    assert!(
        result
            .errors()
            .any(|diagnostic| diagnostic.module.as_str() == path.display().to_string())
    );
}

#[test]
fn entrypoint_schema_reads_direct_function_params() {
    let schema = extract_entrypoint_schema(
        r#"
        return function(target: Node, count: number?)
            return nil
        end
        "#,
    )
    .expect("schema");

    assert_eq!(2, schema.params.len());
    assert_eq!("target", schema.params[0].name);
    assert_eq!("Node", schema.params[0].annotation);
    assert!(!schema.params[0].optional);
    assert_eq!("count", schema.params[1].name);
    assert_eq!("number?", schema.params[1].annotation);
    assert!(schema.params[1].optional);
}

#[test]
fn add_definitions_path_loads_file_contents() {
    let mut checker = Checker::new().expect("checker");
    let path = env::temp_dir().join(format!("ruau-defs-{}.d.luau", process::id()));
    fs::write(&path, "declare function file_defined(): string\n").expect("write");

    checker.add_definitions_path(&path).expect("definitions");
    let result = checker
        .check("local value: string = file_defined()")
        .expect("check");

    fs::remove_file(&path).expect("remove");
    assert!(result.is_ok(), "{result:#?}");
}

#[tokio::test]
async fn checked_load_reuses_resolver_snapshot() {
    let resolver = InMemoryResolver::new()
        .with_module("main", "local dep = require('dep')\nreturn dep.value")
        .with_module("dep", "return { value = 42 }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "main").expect("snapshot");
    let mut checker = Checker::new().expect("checker");
    let lua = Luau::new();

    let value: i32 = lua
        .checked_load(&mut checker, snapshot)
        .expect("checked load")
        .eval()
        .await
        .expect("eval");
    assert_eq!(42, value);
}

#[test]
fn resolver_snapshot_checks_module_graph() {
    let resolver = InMemoryResolver::new()
        .with_module(
            "main",
            "local dep = require('dep')\nlocal _: number = dep.value",
        )
        .with_module("dep", "return { value = 7 }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "main").expect("snapshot");
    let mut checker = Checker::new().expect("checker");

    let result = checker.check_snapshot(&snapshot).expect("check");
    assert!(result.is_ok(), "{result:#?}");
}

#[test]
fn resolver_snapshot_tracks_relative_dependencies() {
    let resolver = InMemoryResolver::new()
        .with_module("app/main", "local dep = require('./dep')\nreturn dep")
        .with_module("app/dep", "return { value = 7 }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "app/main").expect("snapshot");
    let dep = snapshot
        .dependency(&ModuleId::new("app/main"), "./dep")
        .expect("dependency");

    assert_eq!("app/dep", dep.id().as_str());
}

#[tokio::test]
async fn checked_load_resolved_resolves_checks_and_runs() {
    let resolver = InMemoryResolver::new()
        .with_module("main", "local dep = require('dep')\nreturn dep.value")
        .with_module("dep", "return { value = 42 }");
    let mut checker = Checker::new().expect("checker");
    let lua = Luau::new();

    let value: i32 = lua
        .checked_load_resolved(&mut checker, &resolver, "main")
        .expect("checked load")
        .eval()
        .await
        .expect("eval");
    assert_eq!(42, value);
}

#[tokio::test]
async fn checked_load_failure_does_not_mutate_vm_globals() {
    let resolver = InMemoryResolver::new()
        .with_module(
            "main",
            "local dep = require('dep')\nlocal value: number = dep.value\nreturn value",
        )
        .with_module("dep", "return { value = 'wrong' }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "main").expect("snapshot");
    let mut checker = Checker::new().expect("checker");
    let lua = Luau::new();
    lua.globals().set("sentinel", "unchanged").expect("global");

    let error = match lua.checked_load(&mut checker, snapshot) {
        Ok(_) => panic!("checked load should fail"),
        Err(error) => error,
    };

    assert!(matches!(error, AnalysisError::CheckFailed(_)));
    assert_eq!(
        "unchanged",
        lua.globals().get::<String>("sentinel").expect("sentinel")
    );
}

async fn fetch(_lua: &Luau, key: String) -> ruau::Result<String> {
    yield_now().await;
    Ok(format!("value:{key}"))
}

#[tokio::test]
async fn tokio_embedding_uses_async_host_checked_loading_and_modules() {
    let host = HostApi::new().global_async_function(
        "fetch",
        fetch,
        "declare function fetch(key: string): string",
    );
    let resolver = InMemoryResolver::new()
        .with_module("main", "local dep = require('dep')\nreturn fetch(dep.key)")
        .with_module("dep", "return { key = 'project' }");

    let mut checker = Checker::new().expect("checker");
    host.add_definitions_to(&mut checker).expect("definitions");

    let lua = Luau::new();
    host.install(&lua).expect("install");
    let value: String = lua
        .checked_load_resolved(&mut checker, &resolver, "main")
        .expect("checked load")
        .eval()
        .await
        .expect("eval");

    assert_eq!("value:project", value);
}

#[tokio::test]
async fn host_definitions_are_visible_to_analysis_and_runtime() {
    let host = HostApi::new().global_function(
        "log",
        |_lua, message: String| {
            assert_eq!("hello", message);
            Ok(())
        },
        "declare function log(message: string)",
    );

    let mut checker = Checker::new().expect("checker");
    host.add_definitions_to(&mut checker).expect("definitions");
    let result = checker.check("log('hello')").expect("check");
    assert!(result.is_ok(), "{result:#?}");

    let lua = Luau::new();
    host.install(&lua).expect("install");
    lua.load("log('hello')").exec().await.expect("exec");
}
