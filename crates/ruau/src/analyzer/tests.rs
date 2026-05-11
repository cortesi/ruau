use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use tempfile::tempdir;

use super::{
    AnalysisError, CheckResult, Checker, CheckerOptions, Diagnostic, ModuleInterfaceSet, Severity,
    extract_entrypoint_schema, extract_module_schema,
};
use crate::resolver::{ModuleId, SourceSpan};

/// Verifies `CheckResult::is_ok` is true for warning-only results.
#[test]
fn check_result_ok_with_warnings() {
    let result = CheckResult {
        diagnostics: vec![Diagnostic {
            module: ModuleId::new("test"),
            span: SourceSpan {
                line: 0,
                column: 0,
                end_line: 0,
                end_column: 1,
            },
            severity: Severity::Warning,
            message: "unused local".to_owned(),
        }],
        timed_out: false,
        cancelled: false,
    };

    assert!(result.is_ok());
    assert_eq!(1, result.warnings().count());
    assert_eq!(0, result.errors().count());
}

/// Verifies `CheckResult::is_ok` is false when at least one error exists.
#[test]
fn check_result_not_ok_with_error() {
    let result = CheckResult {
        diagnostics: vec![Diagnostic {
            module: ModuleId::new("test"),
            span: SourceSpan {
                line: 1,
                column: 1,
                end_line: 1,
                end_column: 5,
            },
            severity: Severity::Error,
            message: "type mismatch".to_owned(),
        }],
        timed_out: false,
        cancelled: false,
    };

    assert!(!result.is_ok());
    assert_eq!(0, result.warnings().count());
    assert_eq!(1, result.errors().count());
}

/// Verifies checker options defaults use stable module labels.
#[test]
fn checker_options_defaults_are_stable() {
    let options = CheckerOptions::default();
    assert_eq!("main", options.default_module_name);
    assert_eq!("@definitions", options.default_definitions_module_name);
    assert!(options.default_timeout.is_none());
}

/// Verifies schema extraction reads direct function parameters in order.
#[test]
fn extract_entrypoint_schema_reads_params() {
    let schema = extract_entrypoint_schema(
        r#"
return function(target: Node, count: number?, payload: JsonValue)
    return nil
end
"#,
    )
    .expect("schema");
    assert_eq!(3, schema.params.len());
    assert_eq!("target", schema.params[0].name);
    assert_eq!("Node", schema.params[0].annotation);
    assert!(!schema.params[0].optional);
    assert_eq!("count", schema.params[1].name);
    assert_eq!("number?", schema.params[1].annotation);
    assert!(schema.params[1].optional);
    assert_eq!("payload", schema.params[2].name);
    assert_eq!("JsonValue", schema.params[2].annotation);
    assert!(!schema.params[2].optional);
}

/// Verifies schema extraction rejects indirect entrypoints.
#[test]
fn extract_entrypoint_schema_rejects_indirect_return() {
    let error = extract_entrypoint_schema(
        r#"
local main = function(target: Node)
    return nil
end
return main
"#,
    )
    .expect_err("schema should fail");
    assert!(
        error
            .to_string()
            .contains("script must use a direct `return function(...) ... end` entrypoint"),
        "{error}"
    );
}

/// Verifies module schema extraction reads module roots, namespaces, and class methods.
#[test]
fn extract_module_schema_reads_root_and_classes() {
    let schema = extract_module_schema(
        r#"-- Store module.
export type Mode = "read" | "write"

-- Persistent key-value store.
declare class Store
    -- Backing field.
    field: string
    -- Fetch a value.
    function get(self, key: string): string?
    put: (self, key: string, value: string) -> ()
end

declare demo: {
    open: (name: string) -> Store,
    nested: {
        count: () -> number,
    },
}
"#,
    )
    .expect("schema");

    let root = schema.root.expect("module root");
    assert_eq!("demo", root.name);
    assert_eq!(vec!["open"], root.namespace.functions);
    assert_eq!(vec!["count"], root.namespace.children["nested"].functions);
    assert_eq!(
        "string?",
        schema.classes["Store"].method_signatures["get"]
            .returns
            .source
    );
    assert_eq!(
        "Store module.",
        schema.module_description.as_deref().unwrap()
    );
    assert_eq!(
        "Fetch a value.",
        schema.classes["Store"].method_signatures["get"]
            .docs
            .as_deref()
            .unwrap()
    );
    assert_eq!(
        "Backing field.",
        schema.classes["Store"].fields["field"]
            .docs
            .as_deref()
            .unwrap()
    );
    assert!(schema.classes["Store"].fields["field"].span.is_some());
    assert!(
        schema.classes["Store"].method_signatures["get"].args[0]
            .span
            .is_some()
    );
    assert_eq!(vec!["get", "put"], schema.classes["Store"].methods);
}

#[test]
fn extract_module_schema_reads_canonical_module_alias() {
    let schema = extract_module_schema(
        r#"
export type Module = {
    -- Build a package.
    build: (target: string) -> boolean,
}
"#,
    )
    .expect("schema");

    let root = schema.root.expect("module root");
    assert_eq!("Module", root.name);
    assert_eq!("boolean", root.namespace.callables["build"].returns.source);
    assert_eq!(
        "Build a package.",
        root.namespace.callables["build"].docs.as_deref().unwrap()
    );
}

#[test]
fn extract_module_schema_accepts_verber_style_declarations() {
    let verber = r#"
-- Host introspection API for the current Verber runtime session.
export type ModuleBackend = "builtin" | "stdlib" | "mcp_server" | "luau"

export type ProcessInfo = {
    name: string,
    backend: ModuleBackend,
    pid: number?,
    state: string,
    reason: string?,
}

declare verber: {
    version: () -> string,
    processes: () -> {ProcessInfo},
    api: (path: string) -> string?,
}
"#;
    let session = r#"
export type Id = string
export type Outcome =
    { kind: "ok", prints: { string }, value: any }
    | { kind: "runtime_error", prints: { string }, error: any, traceback: string }

declare class Session
    id: Id
    status: string
    function execs(self): { any }
    function note(self): string
end

declare session: {
    current: () -> Session,
    recent: (limit: number?) -> { Session },
    get: (id: Id) -> Session?,
    set_note: (note: string) -> (),
}
"#;
    let config = r#"
export type AccessMode = "read_only" | "read_write"
export type PathGrant = AccessMode | {
    access: AccessMode,
    mcp: boolean?,
}

declare config: {
    args: () -> { [string]: string },
    current: () -> { cwd: string, paths: { [string]: PathGrant } },
}
"#;

    for (name, source) in [("verber", verber), ("session", session), ("config", config)] {
        let schema = extract_module_schema(source).unwrap_or_else(|error| {
            panic!("Verber-style declaration {name} should parse: {error}")
        });
        assert!(
            schema.root.is_some(),
            "{name} should have a root declaration"
        );
    }
}

#[test]
fn extract_module_schema_handles_generated_mcp_punctuation() {
    let source = r#"
export type RepoListItem = {
    ["bad-name"]: boolean?,
    punctuated_enum: ("reactions\x2D\x2D1" | "comments (beta)" | "labels[0]")?,
}

declare github: {
    labels: (args: { owner: string, repo: string }) -> { string },
    repos: {
        list: (args: { query: string? }) -> { RepoListItem },
    },
}
"#;
    let schema = extract_module_schema(source).expect("generated MCP-style schema");
    let root = schema.root.expect("root");
    assert!(root.namespace.callables.contains_key("labels"));
    assert!(
        root.namespace.children["repos"]
            .callables
            .contains_key("list")
    );
    assert!(schema.type_aliases.contains_key("RepoListItem"));
}

#[test]
fn extract_module_schema_accepts_declaration_fixtures() {
    let fixtures = [
        (
            "verber",
            include_str!("../../tests/fixtures/declarations/verber.d.luau"),
        ),
        (
            "fs",
            include_str!("../../tests/fixtures/declarations/fs.d.luau"),
        ),
        (
            "mcp",
            include_str!("../../tests/fixtures/declarations/mcp.d.luau"),
        ),
        (
            "sh",
            include_str!("../../tests/fixtures/declarations/sh.d.luau"),
        ),
        (
            "session",
            include_str!("../../tests/fixtures/declarations/session.d.luau"),
        ),
        (
            "config",
            include_str!("../../tests/fixtures/declarations/config.d.luau"),
        ),
        (
            "generated_mcp",
            include_str!("../../tests/fixtures/declarations/generated_mcp.d.luau"),
        ),
    ];

    for (name, source) in fixtures {
        let schema = extract_module_schema(source)
            .unwrap_or_else(|error| panic!("{name} declaration should parse: {error}"));
        assert!(schema.root.is_some(), "{name} should expose a module root");
    }
}

#[test]
fn extract_module_schema_handles_nested_contract_shapes() {
    let source = r#"
-- Utility module.
export type Result<T> = {
    ok: boolean,
    value: T?,
    errors: { string }?,
}

export type Options = {
    ["literal-key"]: "a,b" | "brace { ok }" | "paren(value)"?,
    callback: ((name: string, values: { [string]: Result<number> }) -> ())?,
}

declare class Handle
    id: string
    function close(self): ()
end

declare tools: {
    make: (name: string, options: Options?) -> Result<number>,
    nested: {
        run: (handles: { Handle }) -> { [string]: Result<number> },
    },
}
"#;

    let schema = extract_module_schema(source).expect("schema");
    let root = schema.root.expect("root");
    assert!(root.namespace.callables.contains_key("make"));
    assert!(
        root.namespace.children["nested"]
            .callables
            .contains_key("run")
    );
    assert!(schema.classes["Handle"].fields.contains_key("id"));
    assert!(
        schema.classes["Handle"]
            .method_signatures
            .contains_key("close")
    );
    assert!(schema.type_aliases.contains_key("Result"));
    assert!(schema.type_aliases.contains_key("Options"));
}

#[test]
fn extract_module_schema_error_display_is_stable() {
    let error = extract_module_schema(
        r#"
declare first: {}
declare second: {}
"#,
    )
    .expect_err("schema should fail");

    assert_eq!(
        error.to_string(),
        "failed to extract Luau module schema: multiple module-root declarations: `first` and `second`"
    );
}

#[tokio::test]
async fn checker_uses_module_interface_set_for_slash_requires() {
    let mut interfaces = ModuleInterfaceSet::new();
    interfaces
        .insert(
            "rust/cargo",
            r#"
export type Module = {
    check: (package: string) -> boolean,
}
"#,
        )
        .expect("interface");

    let mut checker = Checker::new().expect("checker");
    let result = checker
        .check_with_interfaces(
            r#"
local cargo = require("rust/cargo")
local ok: boolean = cargo.check("verber")
return ok
"#,
            &interfaces,
        )
        .await
        .expect("check");
    assert!(result.is_ok(), "diagnostics: {:?}", result.diagnostics);
}

#[tokio::test]
async fn interface_sets_accept_pre_resolved_interfaces() {
    let mut parsed = ModuleInterfaceSet::new();
    parsed
        .insert(
            "demo",
            r#"
declare demo: {
    answer: () -> number,
}
"#,
        )
        .expect("interface");
    let interface = parsed.get("demo").expect("stored interface").clone();

    let mut interfaces = ModuleInterfaceSet::new();
    assert!(interfaces.insert_interface(interface).is_none());

    let mut checker = Checker::new().expect("checker");
    let result = checker
        .check_with_interfaces(
            r#"
local demo = require("demo")
local answer: number = demo.answer()
"#,
            &interfaces,
        )
        .await
        .expect("check");

    assert!(result.is_ok(), "diagnostics: {:?}", result.diagnostics);
}

#[tokio::test]
async fn implementation_interfaces_are_requireable() {
    let mut interfaces = ModuleInterfaceSet::new();
    interfaces.insert_implementation(
        "demo",
        r#"
local M = {}
function M.answer()
    return 42
end
return M
"#,
    );

    let mut checker = Checker::new().expect("checker");
    let result = checker
        .check_with_interfaces(
            r#"
local demo = require("demo")
local answer: number = demo.answer()
"#,
            &interfaces,
        )
        .await
        .expect("check");

    assert!(result.is_ok(), "diagnostics: {:?}", result.diagnostics);
}

#[tokio::test]
async fn check_implementation_compares_return_type_to_declaration() {
    let mut interfaces = ModuleInterfaceSet::new();
    interfaces
        .insert(
            "demo",
            r#"
export type Module = {
    answer: () -> number,
}
"#,
        )
        .expect("interface");

    let mut checker = Checker::new().expect("checker");
    let result = checker
        .check_implementation(
            r#"
local M = {}
function M.answer()
    return "wrong"
end
return M
"#,
            &ModuleId::new("demo.luau"),
            &interfaces,
            "demo",
        )
        .await
        .expect("check");

    assert!(!result.is_ok());
    assert!(
        result
            .errors()
            .any(|diagnostic| diagnostic.message.contains("number"))
    );
}

#[tokio::test]
async fn check_implementation_uses_source_path_for_relative_requires() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path();
    let lib = root.join("lib");
    fs::create_dir_all(&lib).expect("lib dir");
    let main = lib.join("main.luau");
    fs::write(
        lib.join("util.luau"),
        r#"
return {
    add = function(value: number)
        return value + 10
    end,
}
"#,
    )
    .expect("util");
    fs::write(
        root.join("counter.luau"),
        r#"
return {
    next = function()
        return 32
    end,
}
"#,
    )
    .expect("counter");
    fs::write(&main, "return {}\n").expect("main placeholder");

    let mut interfaces = ModuleInterfaceSet::new();
    interfaces
        .insert(
            "lib/main",
            r#"
export type Module = {
    answer: () -> number,
}
"#,
        )
        .expect("interface");

    let mut checker = Checker::new().expect("checker");
    let result = checker
        .check_implementation(
            r#"
local util = require("@self/util")
local counter = require("../counter")

return {
    answer = function()
        return util.add(counter.next())
    end,
}
"#,
            &ModuleId::new(main.display().to_string()),
            &interfaces,
            "lib/main",
        )
        .await
        .expect("check");

    assert!(result.is_ok(), "diagnostics: {:?}", result.diagnostics);
}

#[tokio::test]
async fn check_implementation_uses_virtual_module_id_for_relative_requires() {
    let mut interfaces = ModuleInterfaceSet::new();
    interfaces
        .insert(
            "tools/_search",
            r#"
export type Module = {
    run: () -> number,
}
"#,
        )
        .expect("dependency interface");
    interfaces
        .insert(
            "tools/search",
            r#"
export type Module = {
    answer: () -> number,
}
"#,
        )
        .expect("wrapper interface");

    let mut checker = Checker::new().expect("checker");
    let result = checker
        .check_implementation(
            r#"
local raw = require("./_search")

return {
    answer = raw.run,
}
"#,
            &ModuleId::new("tools/search"),
            &interfaces,
            "tools/search",
        )
        .await
        .expect("check");

    assert!(result.is_ok(), "diagnostics: {:?}", result.diagnostics);
}

/// Verifies module schema extraction rejects multiple module roots.
#[test]
fn extract_module_schema_rejects_multiple_roots() {
    let error = extract_module_schema(
        r#"
declare first: {}
declare second: {}
"#,
    )
    .expect_err("schema should fail");

    assert!(
        error
            .to_string()
            .contains("multiple module-root declarations"),
        "{error}"
    );
}

/// Verifies path-based source checks surface readable file errors.
#[tokio::test]
async fn check_path_reports_read_error() {
    let mut checker = Checker::new().expect("checker creation should succeed");
    let missing = temp_path("missing_source");

    let error = checker
        .check_path(&missing)
        .await
        .expect_err("missing file should fail");
    match error {
        AnalysisError::ReadFile {
            kind,
            path,
            message,
        } => {
            assert_eq!("source", kind);
            assert_eq!(missing.display().to_string(), path);
            assert!(
                !message.is_empty(),
                "read error message should not be empty"
            );
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

/// Verifies path-based definitions loading reads UTF-8 files and preserves labels.
#[tokio::test]
async fn add_definitions_path_loads_file_contents() {
    let mut checker = Checker::new().expect("checker creation should succeed");
    let path = temp_path("definitions");
    fs::write(&path, "declare function file_defined(): string\n")
        .expect("definitions file should be written");

    checker
        .add_definitions_path(&path)
        .expect("definitions path should load");
    let result = checker
        .check(
            r#"
            --!strict
            local value: string = file_defined()
            "#,
        )
        .await
        .expect("source should check");

    fs::remove_file(&path).expect("temp file should be removed");
    assert!(result.is_ok(), "path-loaded definitions should stay active");
}

/// Creates a unique temp file path for filesystem tests.
fn temp_path(stem: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos();
    env::temp_dir().join(format!("ruau-{stem}-{unique}.luau"))
}
