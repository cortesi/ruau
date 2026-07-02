//! Pure Rust implementation of Luau.
//!
//! This is the main crate. It re-exports the parser, source model, checker,
//! bytecode, VM, and VM extension API under stable namespaces.
//!
//! - `ruau::vm`: Runtime-capability-aware compilation, VM building, scoped
//!   values, host functions, and embedding ergonomics.
//! - `ruau::vm_api`: Native-module, host callback, and low-level VM extension API.
//! - `ruau::ast`: Syntax tree, parser, pretty-printer, JSON AST document, and
//!   locations.
//! - `ruau::source`: Module identities and source providers.
//! - `ruau::decl`: Declaration authoring model.
//! - `ruau::fs`: Filesystem-backed module sources and config materializers.
//! - `ruau::analysis`: Static require helpers and source graphs.
//! - `ruau::typecheck`: Checker, diagnostics, schemas, and source queries.
//! - `ruau::bytecode`: Bytecode model, codec, validation, and raw compiler APIs.
//! - `ruau::surface`: Validated runtime and checker surface configuration.
//! - `ruau::host`: Retained source-eval host over a validated surface.
//! - `ruau::runner`: Native bounded multi-tenant request runner.
//!
//! The optional `derive` feature enables `IntoLua`/`FromLua` derives.
//!
//! # Export paths
//!
//! Each Ruau workspace item has one public path. Workspace crates are exposed
//! as namespaces here, but items are not re-exported between those namespaces.
//! If a type belongs to `ruau::vm`, name it there; if it belongs to
//! `ruau::surface`, name it there. The exception is foreign types that appear
//! in public signatures, which may be re-exported at their point of use so
//! embedders do not need to add a dependency only to name a return or argument
//! type.

/// Static analysis config, source graphs, and require helpers.
pub use ruau_analysis as analysis;
/// Parser, AST, AST JSON, pretty-printer, locations, and visitors.
pub use ruau_ast as ast;
/// Bytecode model, codec, validation, and raw compiler APIs.
pub use ruau_bytecode as bytecode;
/// Typed Luau declaration authoring model.
pub use ruau_decl as decl;
/// Filesystem-backed module sources and config materializers.
pub use ruau_fs as fs;
/// Retained source-eval host over a validated surface.
pub use ruau_host as host;
/// Module identity and async-first module source reads.
pub use ruau_source as source;
/// Validated runtime and checker surface configuration.
pub use ruau_surface as surface;
/// Type checking, diagnostics, schema extraction, views, and source queries.
pub use ruau_typecheck as typecheck;
/// Runtime, VM building, scoped values, host functions, and embedding ergonomics.
pub use ruau_vm as vm;
/// Native-module, host callback, and low-level VM extension API.
pub use ruau_vm_api as vm_api;

/// RuntimeCapabilities selection and checker/compiler alignment helpers.
#[cfg(any())]
mod capabilities {
    use crate::vm::{Library, RuntimeCapabilities};

    /// Builds a type-checker builtin environment matching a VM [`RuntimeCapabilities`].
    ///
    /// Globals for omitted libraries are removed, so references to disabled
    /// libraries are rejected during checking.
    #[must_use]
    pub fn builtin_environment_for(
        capabilities: &RuntimeCapabilities,
        arena: &mut crate::typecheck::types::Arena,
    ) -> crate::typecheck::builtins::BuiltinEnvironment {
        builtin_environment_for_with_definition_modules(capabilities, arena, &[])
    }

    /// Builds a capability-selected builtin environment plus audited host-module
    /// declaration modules.
    #[must_use]
    pub fn builtin_environment_for_with_definition_modules(
        capabilities: &RuntimeCapabilities,
        arena: &mut crate::typecheck::types::Arena,
        definition_modules: &[crate::typecheck::builtins::DefinitionModule],
    ) -> crate::typecheck::builtins::BuiltinEnvironment {
        crate::typecheck::builtins::BuiltinEnvironment::standard_with_definition_modules(
            arena,
            definition_modules,
        )
        .without_globals(
            capabilities
                .omitted_libraries()
                .map(Library::global_name)
                .chain((!capabilities.runtime_compilation_enabled()).then_some("loadstring")),
        )
    }
}

/// Native multi-tenant request runner.
///
/// This namespace is native-only; wasm embedders drive one VM directly from
/// their host loop.
#[cfg(not(target_arch = "wasm32"))]
pub use ruau_runner as runner;

/// A fully-specified VM builder for ruau's own tests: the historical defaults
/// (a `deterministic(0)` ambient, default limits, full runtime capabilities)
/// made explicit and individually overridable, so a test exercising one field
/// need not re-state the other two now that [`ruau_vm::VmBuilder::build`] fails
/// closed.
#[cfg(any())]
pub(crate) fn test_vm_builder() -> ruau_vm::VmBuilder {
    ruau_vm::Vm::builder()
        .ambient(ruau_vm::Ambient::deterministic(0))
        .limits(ruau_vm::Limits::unlimited())
        .runtime_capabilities(ruau_vm::RuntimeCapabilities::default().enable_runtime_compilation())
        .trusted_host()
}

#[cfg(any())]
mod tests {
    fn capabilities_without_library(library: crate::vm::Library) -> crate::vm::RuntimeCapabilities {
        crate::vm::RuntimeCapabilities::from_libraries(
            crate::vm::Library::ALL
                .iter()
                .copied()
                .filter(move |candidate| *candidate != library),
        )
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn compile_thread_migration_source(source: &str) -> crate::bytecode::BytecodeChunk {
        ruau_bytecode::compile_source(source, &crate::bytecode::CompileOptions::default(), None)
            .expect("test compiles")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn raw_number(value: ruau_vm_api::RawValue) -> f64 {
        match value {
            ruau_vm_api::RawValue::Number(n) => n,
            ruau_vm_api::RawValue::Integer(i) => i as f64,
            other => panic!("expected a numeric result, got {other:?}"),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_vm_at_rest_moves_across_threads_between_calls() {
        let chunk = compile_thread_migration_source("count = (count or 0) + 1 return count");
        let bump = |vm: &mut ruau_vm::Vm| -> f64 {
            let module = vm.load(&chunk).expect("counter module loads");
            raw_number(
                vm.call(&module, Default::default())
                    .expect("counter call succeeds")[0],
            )
        };

        let mut control = crate::test_vm_builder().build().expect("test vm builds");
        let reference = [bump(&mut control), bump(&mut control), bump(&mut control)];

        let mut vm = crate::test_vm_builder().build().expect("test vm builds");
        let first = bump(&mut vm);
        let (mut vm, second) = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let second = bump(&mut vm);
                    (vm, second)
                })
                .join()
                .expect("worker thread completes")
        });
        let third = bump(&mut vm);

        assert_eq!([first, second, third], reference);
        vm.validate().expect("heap valid after migrating");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn vm_stays_send_for_thread_migration() {
        fn assert_send<T: Send>() {}
        assert_send::<ruau_vm::Vm>();
    }

    /// The stdlib conformance gate: every member a library's
    /// `.d.luau` declares must be installed at runtime, and vice versa, so a
    /// type-checked program can never call a declared-but-`nil` member nor reach
    /// an installed-but-undeclared one. This pins the `.d.luau`↔engine-builtin
    /// surface that the typechecker and the VM must agree on.
    mod conformance_gate {
        use std::collections::BTreeSet;

        use ruau_typecheck::builtins::fixture_defs as defs;
        use ruau_vm::{NextStep, Vm};
        use ruau_vm_api::RawValue;

        /// The top-level member names a `declare <lib>: { ... }` block declares.
        /// Scoped to the balanced `declare` block (so a preceding `type X = {...}`
        /// alias is ignored), then the 4-space-indented `name:` lines within it.
        fn declared_members(lib: &str, src: &str) -> BTreeSet<String> {
            let needle = format!("declare {lib}:");
            let start = src
                .find(&needle)
                .unwrap_or_else(|| panic!("{lib}: no `declare {lib}:` block"));
            let open = start
                + src[start..]
                    .find('{')
                    .expect("declare block has an opening brace");
            // Walk to the matching close brace.
            let mut depth = 0i32;
            let mut end = open;
            for (offset, ch) in src[open..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + offset;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let block = &src[open..=end];
            // Walk the block tracking `{`-brace depth so only members directly
            // inside the `declare` table (depth 1) count — a member whose type is a
            // nested inline table cannot leak its fields in as top-level members.
            let mut members = BTreeSet::new();
            let mut depth = 0i32;
            for line in block.lines() {
                let depth_before = depth;
                for ch in line.chars() {
                    match ch {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                if depth_before != 1 {
                    continue;
                }
                // A top-level member is the line's leading `<ident>:` (an inline
                // table type's fields sit mid-line, never at the line start).
                let name: String = line
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let after = line.trim_start()[name.len()..].trim_start();
                if !name.is_empty() && after.starts_with(':') {
                    members.insert(name);
                }
            }
            members
        }

        /// The member names installed on a library's global table at runtime.
        fn installed_members(vm: &mut Vm, lib: &str) -> BTreeSet<String> {
            let key = RawValue::String(
                vm.heap_mut()
                    .intern_str(lib.as_bytes())
                    .expect("intern library name"),
            );
            let globals = vm.globals().expect("globals table installed");
            let lib_value = vm.heap().table(globals).expect("globals resident").get(key);
            let RawValue::Table(handle) = lib_value else {
                panic!("{lib} is not an installed library table: {lib_value:?}");
            };
            let mut members = BTreeSet::new();
            let mut cursor = RawValue::Nil;
            loop {
                let step = vm
                    .heap()
                    .table(handle)
                    .expect("library resident")
                    .next(cursor);
                match step {
                    NextStep::Pair(k, _) => {
                        if let RawValue::String(name) = k {
                            let bytes = vm.heap().string(name).expect("key resident").bytes();
                            members.insert(String::from_utf8_lossy(bytes).into_owned());
                        }
                        cursor = k;
                    }
                    NextStep::Done | NextStep::InvalidKey => break,
                }
            }
            members
        }

        /// Members declared in a `.d.luau` but deliberately not yet installed.
        /// Empty: the whole declared stdlib surface is now installed. The gate
        /// keeps any future entry honest — it must be a subset of what is declared
        /// and disjoint from what is installed.
        const DEFERRED: &[(&str, &str)] = &[];

        #[test]
        fn installed_library_members_match_their_declarations() {
            let libraries = [
                ("bit32", defs::BIT32),
                ("buffer", defs::BUFFER),
                ("coroutine", defs::COROUTINE),
                ("debug", defs::DEBUG),
                ("math", defs::MATH),
                ("os", defs::OS),
                ("string", defs::STRING),
                ("table", defs::TABLE),
                ("utf8", defs::UTF8),
                ("vector", defs::VECTOR),
            ];
            let mut vm = crate::test_vm_builder().build().expect("test vm builds");
            let mut failures = Vec::new();
            for (lib, src) in libraries {
                let deferred: BTreeSet<String> = DEFERRED
                    .iter()
                    .filter(|(l, _)| *l == lib)
                    .map(|(_, m)| (*m).to_owned())
                    .collect();
                let declared = declared_members(lib, src);
                let installed = installed_members(&mut vm, lib);
                // Every deferred member must be declared and must NOT be installed.
                let stale: Vec<_> = deferred.difference(&declared).cloned().collect();
                if !stale.is_empty() {
                    failures.push(format!(
                        "{lib}: DEFERRED lists undeclared members {stale:?}"
                    ));
                }
                let resurrected: Vec<_> = deferred.intersection(&installed).cloned().collect();
                if !resurrected.is_empty() {
                    failures.push(format!(
                        "{lib}: {resurrected:?} are installed now — remove them from DEFERRED"
                    ));
                }
                // The installed surface must equal the declared surface minus the
                // tracked deferrals.
                let expected: BTreeSet<String> = declared.difference(&deferred).cloned().collect();
                let missing: Vec<_> = expected.difference(&installed).cloned().collect();
                let extra: Vec<_> = installed.difference(&expected).cloned().collect();
                if !missing.is_empty() || !extra.is_empty() {
                    failures.push(format!(
                        "{lib}: declared-but-not-installed {missing:?}; installed-but-not-declared {extra:?}"
                    ));
                }
            }
            assert!(
                failures.is_empty(),
                "stdlib conformance gate:\n{}",
                failures.join("\n")
            );
        }
    }

    #[test]
    fn exposes_checker_entrypoint_through_typecheck_mount() {
        let mut checker = crate::typecheck::Checker::new();

        let checked = checker.check_source("--!strict\nlocal x = 1\nreturn x");

        assert!(!checked.has_errors());
        let exports: &crate::typecheck::ModuleExports = checked.exports();
        assert!(exports.is_empty());
    }

    /// Whether `source` type-checks cleanly against the checker environment for
    /// the selected runtime capabilities.
    fn checks_clean(capabilities: &ruau_vm::RuntimeCapabilities, source: &str) -> bool {
        let mut arena = crate::typecheck::types::Arena::new();
        let builtins = crate::capabilities::builtin_environment_for(capabilities, &mut arena);
        let mut checker = crate::typecheck::Checker::with_builtins(arena, builtins);
        !checker.check_source(source).has_errors()
    }

    #[test]
    fn runtime_capabilities_restrict_the_checker_builtin_environment() {
        use ruau_vm::Library;

        // Under the full capability set, referencing `os` type-checks.
        assert!(checks_clean(
            &ruau_vm::RuntimeCapabilities::default().enable_runtime_compilation(),
            "--!strict\nlocal x = os\nreturn x"
        ));

        // Capabilities that omit `os` make a reference to it an unknown symbol,
        // while a still-enabled library (`math`) keeps resolving.
        let no_os = capabilities_without_library(Library::Os);
        assert!(!checks_clean(&no_os, "--!strict\nlocal x = os\nreturn x"));
        assert!(checks_clean(&no_os, "--!strict\nlocal x = math\nreturn x"));
    }

    #[test]
    fn runtime_capabilities_suppress_a_disabled_library_constant_fold() {
        use ruau_bytecode::{CompileOptions, compile_source};
        use ruau_vm::Library;
        use ruau_vm_api::RawValue;

        let no_math = capabilities_without_library(Library::Math);
        // Library-constant folding is on at optimization level 2.
        let folding = || CompileOptions {
            optimization_level: 2,
            ..CompileOptions::default()
        };

        // At level 2 the compiler folds `math.pi` to a constant, so it returns a
        // value even in a VM that never installed `math` — the leak to close.
        let folded = compile_source("return math.pi", &folding(), None).expect("compile");
        let mut leaky = crate::test_vm_builder()
            .runtime_capabilities(no_math.clone())
            .build()
            .expect("test vm builds");
        let module = leaky.load(&folded).expect("load");
        assert!(
            matches!(
                leaky.call(&module, Default::default()).as_deref(),
                Ok([RawValue::Number(_)])
            ),
            "level-2 folding bakes math.pi in, leaking it past a disabled library"
        );

        // RuntimeCapabilities-aware compilation suppresses the fold, so `math.pi` reads
        // the absent global and fails closed.
        let unfolded = no_math
            .compile_source("return math.pi".as_bytes(), &folding())
            .expect("compile");
        let mut vm = crate::test_vm_builder()
            .runtime_capabilities(no_math)
            .build()
            .expect("test vm builds");
        let module = vm.load(&unfolded).expect("load");
        assert!(
            vm.call(&module, Default::default()).is_err(),
            "with math omitted, the un-folded math.pi must fail closed"
        );
    }

    #[test]
    fn runtime_capabilities_resist_optimize_hot_comment_escalation() {
        use ruau_bytecode::{CompileOptions, compile_source};
        use ruau_vm::Library;
        use ruau_vm_api::RawValue;

        let no_bit32 = capabilities_without_library(Library::Bit32);
        // A pure builtin fold (not just a member constant) under a hot comment that
        // raises the optimization level to 2 regardless of the host's setting.
        let src = "--!optimize 2\nreturn bit32.band(7, 3)";

        // Plain compile at the host default (level 1) still folds, because the
        // source's hot comment raises the level — leaking the disabled bit32.
        let leaked = compile_source(src, &CompileOptions::default(), None).expect("compile");
        let mut leaky = crate::test_vm_builder()
            .runtime_capabilities(no_bit32.clone())
            .build()
            .expect("test vm builds");
        let module = leaky.load(&leaked).expect("load");
        assert!(
            matches!(
                leaky.call(&module, Default::default()).as_deref(),
                Ok([RawValue::Number(_)])
            ),
            "a --!optimize 2 hot comment folds bit32.band past a disabled bit32"
        );

        // RuntimeCapabilities-aware compilation applies the restriction before compiling,
        // so the fold is suppressed even under the hot comment and the call
        // fails closed.
        let safe = no_bit32
            .compile_source(src.as_bytes(), &CompileOptions::default())
            .expect("compile");
        let mut vm = crate::test_vm_builder()
            .runtime_capabilities(no_bit32)
            .build()
            .expect("test vm builds");
        let module = vm.load(&safe).expect("load");
        assert!(
            vm.call(&module, Default::default()).is_err(),
            "runtime-capability-aware compilation closes the hot-comment fold leak"
        );
    }
}
