//! Integrated analyzer API tests.
#![allow(clippy::tests_outside_test_module)]

use std::{cell::Cell, env, fs, path::PathBuf, process, rc::Rc, time::Duration};

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

#[tokio::test]
async fn strict_type_mismatch_reports_error_without_hot_comment() {
    let mut checker = Checker::new().expect("checker");
    let result = checker
        .check(
            r#"
            local x: number = "hello"
            "#,
        )
        .await
        .expect("check");

    assert!(!result.is_ok());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error)
    );
}

#[tokio::test]
async fn definitions_change_check_behavior() {
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
        .await
        .expect("check");
    assert!(ok.is_ok(), "{ok:#?}");

    let bad = checker
        .check(
            r#"
            local _todo = Todo.create():content(42):save()
            "#,
        )
        .await
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

#[tokio::test]
async fn timeout_and_cancellation_are_reported() {
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
        .await
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
        .await
        .expect("check");
    assert!(cancelled.cancelled);
    assert!(!cancelled.is_ok());
}

#[tokio::test]
async fn virtual_and_filesystem_modules_resolve() {
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
        .await
        .expect("virtual check");
    assert!(virtual_result.is_ok(), "{virtual_result:#?}");

    let root = fixture("modules/filesystem/requirer.luau");
    let filesystem_result = checker.check_path(&root).await.expect("filesystem check");
    assert!(filesystem_result.is_ok(), "{filesystem_result:#?}");
}

#[tokio::test]
async fn diagnostics_include_module_identity() {
    let mut checker = Checker::new().expect("checker");
    let path = env::temp_dir().join(format!("ruau-bad-source-{}.luau", process::id()));
    fs::write(&path, "local value: number = 'wrong'\n").expect("write");

    let result = checker.check_path(&path).await.expect("check");

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

#[tokio::test]
async fn add_definitions_path_loads_file_contents() {
    let mut checker = Checker::new().expect("checker");
    let path = env::temp_dir().join(format!("ruau-defs-{}.d.luau", process::id()));
    fs::write(&path, "declare function file_defined(): string\n").expect("write");

    checker.add_definitions_path(&path).expect("definitions");
    let result = checker
        .check("local value: string = file_defined()")
        .await
        .expect("check");

    fs::remove_file(&path).expect("remove");
    assert!(result.is_ok(), "{result:#?}");
}

#[tokio::test]
async fn checked_load_reuses_resolver_snapshot() {
    let resolver = InMemoryResolver::new()
        .with_module("main", "local dep = require('dep')\nreturn dep.value")
        .with_module("dep", "return { value = 42 }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "main")
        .await
        .expect("snapshot");
    let mut checker = Checker::new().expect("checker");
    let lua = Luau::new();

    let value: i32 = lua
        .checked_load(&mut checker, snapshot)
        .await
        .expect("checked load")
        .eval()
        .await
        .expect("eval");
    assert_eq!(42, value);
}

#[tokio::test]
async fn resolver_snapshot_checks_module_graph() {
    let resolver = InMemoryResolver::new()
        .with_module("main", "local dep = require('dep')\nlocal _: number = dep.value")
        .with_module("dep", "return { value = 7 }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "main")
        .await
        .expect("snapshot");
    let mut checker = Checker::new().expect("checker");

    let result = checker.check_snapshot(&snapshot).await.expect("check");
    assert!(result.is_ok(), "{result:#?}");
}

#[tokio::test]
async fn resolver_snapshot_tracks_relative_dependencies() {
    let resolver = InMemoryResolver::new()
        .with_module("app/main", "local dep = require('./dep')\nreturn dep")
        .with_module("app/dep", "return { value = 7 }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "app/main")
        .await
        .expect("snapshot");
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
        .await
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
    let snapshot = ResolverSnapshot::resolve(&resolver, "main")
        .await
        .expect("snapshot");
    let mut checker = Checker::new().expect("checker");
    let lua = Luau::new();
    lua.globals().set("sentinel", "unchanged").expect("global");

    let error = match lua.checked_load(&mut checker, snapshot).await {
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
    let host =
        HostApi::new().global_async_function("fetch", fetch, "declare function fetch(key: string): string");
    let resolver = InMemoryResolver::new()
        .with_module("main", "local dep = require('dep')\nreturn fetch(dep.key)")
        .with_module("dep", "return { key = 'project' }");

    let mut checker = Checker::new().expect("checker");
    host.add_definitions_to(&mut checker).expect("definitions");

    let lua = Luau::new();
    host.install(&lua).expect("install");
    let value: String = lua
        .checked_load_resolved(&mut checker, &resolver, "main")
        .await
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
    let result = checker.check("log('hello')").await.expect("check");
    assert!(result.is_ok(), "{result:#?}");

    let lua = Luau::new();
    host.install(&lua).expect("install");
    lua.load("log('hello')").exec().await.expect("exec");
}

#[tokio::test]
async fn host_api_installs_local_captures_into_multiple_vms() {
    let calls = Rc::new(Cell::new(0));
    let host = HostApi::new().global_function(
        "count",
        {
            let calls = Rc::clone(&calls);
            move |_lua, ()| {
                calls.set(calls.get() + 1);
                Ok(())
            }
        },
        "declare function count()",
    );

    let mut checker_a = Checker::new().expect("checker");
    let mut checker_b = Checker::new().expect("checker");
    host.add_definitions_to(&mut checker_a).expect("definitions");
    host.add_definitions_to(&mut checker_b).expect("definitions");

    let lua_a = Luau::new();
    let lua_b = Luau::new();
    host.install(&lua_a).expect("install a");
    host.install(&lua_b).expect("install b");

    lua_a.load("count()").exec().await.expect("exec a");
    lua_b.load("count()").exec().await.expect("exec b");

    assert_eq!(2, calls.get());
}

#[tokio::test]
async fn host_api_namespace_generates_declaration_and_installs_table() {
    let host = HostApi::new().namespace("term", |ns| {
        ns.function(
            "echo",
            |_lua, msg: String| Ok(format!("term.echo({msg})")),
            "(msg: string) -> string",
        );
        ns.function(
            "len",
            |_lua, msg: String| Ok(msg.len() as i64),
            "(msg: string) -> number",
        );
    });

    // The generated declaration is a single `declare term: { ... }` block.
    let defs = host.definitions();
    assert!(defs.starts_with("declare term:"), "{defs}");
    assert!(defs.contains("echo: (msg: string) -> string"), "{defs}");
    assert!(defs.contains("len: (msg: string) -> number"), "{defs}");

    // Analyzer accepts namespaced calls against the generated declaration.
    let mut checker = Checker::new().expect("checker");
    host.add_definitions_to(&mut checker).expect("definitions");
    let result = checker
        .check("local s = term.echo('hi'); return term.len(s)")
        .await
        .expect("check");
    assert!(result.is_ok(), "{result:#?}");

    // Runtime install creates a read-only `term` table with the registered functions.
    let lua = Luau::new();
    host.install(&lua).expect("install");
    let value: String = lua.load("return term.echo('ok')").eval().await.expect("eval");
    assert_eq!("term.echo(ok)", value);

    // The installed table is read-only.
    let res = lua
        .load("term.echo = function() return 'tampered' end")
        .exec()
        .await;
    assert!(res.is_err(), "expected read-only table error");
}

#[tokio::test]
async fn host_api_namespace_supports_nested_namespaces() {
    let host = HostApi::new().namespace("app", |ns| {
        ns.namespace("term", |term| {
            term.function(
                "print",
                |_lua, msg: String| Ok(msg.to_uppercase()),
                "(msg: string) -> string",
            );
        });
    });

    let defs = host.definitions();
    assert!(
        defs.contains("term: { print: (msg: string) -> string }"),
        "{defs}"
    );

    let mut checker = Checker::new().expect("checker");
    host.add_definitions_to(&mut checker).expect("definitions");
    let result = checker
        .check("return app.term.print('hello')")
        .await
        .expect("check");
    assert!(result.is_ok(), "{result:#?}");

    let lua = Luau::new();
    host.install(&lua).expect("install");
    let value: String = lua
        .load("return app.term.print('hello')")
        .eval()
        .await
        .expect("eval");
    assert_eq!("HELLO", value);
}
