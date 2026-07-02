//! `ModuleBinding` global-binding control: builtin override semantics and
//! host-only (hidden) bindings, end to end across the surface, the checker,
//! and the VM.
//!
//! The specified semantics (replacing the previously unspecified behavior,
//! which was a silent last-wins replacement at runtime with the checker
//! keeping the builtin's signature):
//!
//! - `ModuleBinding::Global` colliding with a surface builtin fails closed at
//!   surface validation and at VM build.
//! - `ModuleBinding::GlobalOverride` is the explicit opt-in: the binding
//!   replaces the builtin before `Vm::sandbox` freezes the globals, and the
//!   module's `.d.luau` declaration replaces the builtin's type in the
//!   checker environment.
//! - `ModuleBinding::Hidden` registers a host-only table in the VM's named
//!   registry: never a script-visible global, fetchable by the host via
//!   `Scope::named_get`, with the declaration contributing only types.
#![allow(clippy::tests_outside_test_module)]

use std::sync::{Arc, Mutex};

use ruau::{
    bytecode::CompileOptions,
    surface::{Surface, VmConfig},
    vm::{Ambient, Limits, ModuleBuilderExt, RuntimeCapabilities, RuntimeError, VmBuildError},
    vm_api::{ModuleBinding, ModuleBuilder, ModuleExport, NativeModule},
};

/// The motivating eguidev case: a strict host `assert` with the signature
/// `(boolean, string?) -> ()` replacing the builtin
/// `assert<T>(value: T, message: string?): T`.
struct StrictAssertModule {
    binding: ModuleBinding,
}

impl StrictAssertModule {
    fn overriding() -> Self {
        Self {
            binding: ModuleBinding::GlobalOverride,
        }
    }

    fn colliding() -> Self {
        Self {
            binding: ModuleBinding::Global,
        }
    }
}

impl NativeModule for StrictAssertModule {
    fn name(&self) -> &'static str {
        "strict_assert"
    }

    fn declaration(&self) -> ruau_decl::DeclSource<'_> {
        ruau_decl::DeclSource::Text({
            "declare function assert(value: boolean, message: string?): ()"
        })
    }

    fn build(&self, builder: &mut dyn ModuleBuilder) {
        builder.leaf_function("assert", self.binding.clone(), |(): ()| 99.0_f64);
    }
}

/// A hidden method table plus a type-only declaration: the declaration
/// defines no global, only the alias describing the table's shape.
struct HiddenMethodsModule;

impl NativeModule for HiddenMethodsModule {
    fn name(&self) -> &'static str {
        "widget_methods"
    }

    fn declaration(&self) -> ruau_decl::DeclSource<'_> {
        ruau_decl::DeclSource::Text("type WidgetMethods = { ping: () -> number }")
    }

    fn build(&self, builder: &mut dyn ModuleBuilder) {
        builder.leaf_function("ping", ModuleBinding::hidden("widget_methods"), |(): ()| {
            7.0_f64
        });
    }
}

struct NativeRequireModule {
    export: ModuleExport,
}

impl NativeRequireModule {
    fn new(export: ModuleExport) -> Self {
        Self { export }
    }
}

impl NativeModule for NativeRequireModule {
    fn name(&self) -> &'static str {
        "native"
    }

    fn declaration(&self) -> ruau_decl::DeclSource<'_> {
        ruau_decl::DeclSource::Text("declare native: { answer: () -> number }")
    }

    fn export(&self) -> ModuleExport {
        self.export
    }

    fn build(&self, builder: &mut dyn ModuleBuilder) {
        builder.leaf_function("answer", ModuleBinding::library("native"), |(): ()| {
            42.0_f64
        });
    }
}

fn deterministic_vm(surface: &Surface) -> ruau::vm::Vm {
    surface
        .vm_builder(&VmConfig::deterministic(0))
        .build()
        .expect("surface-aligned VM builds and sandboxes")
}

fn native_require_vm(export: ModuleExport) -> ruau::vm::Vm {
    ruau::vm::Vm::builder()
        .ambient(Ambient::deterministic(0))
        .limits(Limits::unlimited())
        .runtime_capabilities(RuntimeCapabilities::default().enable_runtime_compilation())
        .module(Arc::new(NativeRequireModule::new(export)))
        .sandboxed()
        .build()
        .expect("native require VM builds")
}

fn run_vm_source(vm: &mut ruau::vm::Vm, source: &[u8]) -> String {
    let chunk = RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(source, &CompileOptions::default())
        .expect("compiles");
    let module = vm.load(&chunk).expect("loads");
    let values = vm
        .call_protected(&module, Default::default())
        .expect("not fatal")
        .expect("not a script error");
    format!("{values:?}")
}

fn strict_has_errors(surface: &Surface, source: &str) -> bool {
    surface.new_checker().check_source(source).has_errors()
}

#[test]
fn require_only_native_module_seeds_require_without_a_global() {
    let mut vm = native_require_vm(ModuleExport::Require);
    let result = run_vm_source(
        &mut vm,
        b"local required = require(\"native\")\nreturn required.answer(), native == nil",
    );
    assert_eq!(
        result, "[Number(42.0), Boolean(true)]",
        "require-only modules are returned from require without installing a global"
    );
}

#[test]
fn both_native_module_seeds_require_and_installs_the_binding() {
    let mut vm = native_require_vm(ModuleExport::Both);
    let result = run_vm_source(
        &mut vm,
        b"local required = require(\"native\")\nreturn required.answer(), native.answer()",
    );
    assert_eq!(
        result, "[Number(42.0), Number(42.0)]",
        "Both exposes the same native table through require and the library binding"
    );
}

#[test]
fn native_module_source_collision_fails_closed() {
    let source = ruau::source::InMemorySource::new()
        .with_module(ruau::source::ModuleId::new("native"), "return {}");
    let mut vm = ruau::vm::Vm::builder()
        .ambient(Ambient::deterministic(0))
        .limits(Limits::unlimited())
        .runtime_capabilities(RuntimeCapabilities::default().enable_runtime_compilation())
        .module(Arc::new(NativeRequireModule::new(ModuleExport::Require)))
        .module_source(Arc::new(source))
        .sandboxed()
        .build()
        .expect("VM builds with a native/source collision configured");
    let chunk = RuntimeCapabilities::default()
        .enable_runtime_compilation()
        .compile_source(b"return require(\"native\")", &CompileOptions::default())
        .expect("compiles");
    let module = vm.load(&chunk).expect("loads");
    let error = vm
        .call_protected(&module, Default::default())
        .expect("not fatal")
        .expect_err("native/source collisions raise a script error");
    assert_eq!(
        error.kind(),
        ruau::vm_api::RuntimeErrorKind::UnresolvedRequire,
        "native/source collisions are reported as require resolution failures"
    );
}

#[test]
fn require_only_native_module_types_require_without_a_global() {
    let surface = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(NativeRequireModule::new(ModuleExport::Require)))
        .build()
        .expect("require-only native module surface validates");

    assert!(
        !strict_has_errors(
            &surface,
            "--!strict\nlocal required = require(\"native\")\nlocal answer: number = required.answer()\nreturn answer\n"
        ),
        "require-only module tables are typed through literal require"
    );
    assert!(
        strict_has_errors(&surface, "--!strict\nreturn native.answer()\n"),
        "require-only module tables are not checker-visible globals"
    );
}

#[test]
fn both_native_module_types_require_and_global_access() {
    let surface = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(NativeRequireModule::new(ModuleExport::Both)))
        .build()
        .expect("both-mode native module surface validates");

    assert!(
        !strict_has_errors(
            &surface,
            "--!strict\nlocal required = require(\"native\")\nlocal answer: number = required.answer() + native.answer()\nreturn answer\n"
        ),
        "both-mode module tables are typed through require and global access"
    );
}

#[test]
fn require_module_surface_audit_rejects_stray_declared_globals() {
    struct BadRequireDeclaration;

    impl NativeModule for BadRequireDeclaration {
        fn name(&self) -> &str {
            "native"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text({
                "declare native: { answer: () -> number }\ndeclare leaked: number"
            })
        }

        fn export(&self) -> ModuleExport {
            ModuleExport::Require
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.leaf_function("answer", ModuleBinding::library("native"), |(): ()| {
                42.0_f64
            });
        }
    }

    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(BadRequireDeclaration))
        .build()
        .expect_err("require-only module declarations must describe only the export table");
    assert!(
        error
            .to_string()
            .contains("declares globals not registered at runtime"),
        "the audit reports the stray global: {error}"
    );
}

#[test]
fn accidental_collision_fails_surface_validation_closed() {
    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(StrictAssertModule::colliding()))
        .build()
        .expect_err("a Global binding colliding with a builtin must fail validation");
    let message = error.to_string();
    assert!(
        message.contains("assert") && message.contains("GlobalOverride"),
        "the error names the colliding global and the opt-in: {message}"
    );
}

#[test]
fn accidental_collision_fails_vm_build_closed() {
    // The VM-level backstop, independent of surface validation.
    let error = ruau::vm::Vm::builder()
        .ambient(Ambient::deterministic(0))
        .limits(Limits::unlimited())
        .runtime_capabilities(RuntimeCapabilities::default().enable_runtime_compilation())
        .module(Arc::new(StrictAssertModule::colliding()))
        .sandboxed()
        .build();
    let Err(VmBuildError::ModuleInstall(error)) = error else {
        panic!("a Global binding colliding with a builtin must fail the build");
    };
    let message = error.to_string();
    assert!(
        message.contains("`assert`") && message.contains("GlobalOverride"),
        "the error names the colliding global and the opt-in: {message}"
    );
}

#[test]
fn override_without_builtin_target_fails_surface_validation() {
    struct NoTargetModule;
    impl NativeModule for NoTargetModule {
        fn name(&self) -> &'static str {
            "no_target"
        }
        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare function no_such_builtin(): number")
        }
        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.leaf_function(
                "no_such_builtin",
                ModuleBinding::GlobalOverride,
                |(): ()| 1.0_f64,
            );
        }
    }

    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(NoTargetModule))
        .build()
        .expect_err("an override with no builtin to replace must fail validation");
    assert!(
        error.to_string().contains("no builtin of that name"),
        "the error says the override target is missing: {error}"
    );
}

#[test]
fn sandboxed_scripts_call_the_override() {
    let surface = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(StrictAssertModule::overriding()))
        .build()
        .expect("an explicit override validates");
    let mut vm = deterministic_vm(&surface);
    let chunk = surface.compile_bytes(b"return assert()").expect("compiles");
    let module = vm.load(&chunk).expect("loads");
    let result = vm
        .call_protected(&module, Default::default())
        .expect("not fatal")
        .expect("the override runs in the sandboxed VM");
    assert_eq!(
        format!("{result:?}"),
        "[Number(99.0)]",
        "the sandboxed script's assert is the host override"
    );
}

#[test]
fn checker_enforces_the_override_signature() {
    let surface = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(StrictAssertModule::overriding()))
        .build()
        .expect("an explicit override validates");

    // The builtin signature is generic (`assert<T>(value: T, ...)`): a number
    // first argument checks against the builtin but violates the override's
    // `boolean`. With the override installed, strict scripts written against
    // the old signature fail...
    assert!(
        strict_has_errors(&surface, "--!strict\nassert(5)\n"),
        "the override's (boolean, string?) signature rejects assert(5)"
    );
    // ...and scripts written against the override's signature pass.
    assert!(
        !strict_has_errors(&surface, "--!strict\nassert(true, \"ok\")\n"),
        "the override's signature accepts assert(true, message)"
    );

    // Control: without the module, the builtin's generic signature accepts
    // the same script the override rejects.
    let plain = Surface::builder()
        .enable_runtime_compilation()
        .build()
        .expect("plain surface validates");
    assert!(
        !strict_has_errors(&plain, "--!strict\nassert(5)\n"),
        "the builtin assert accepts a number first argument"
    );
}

#[test]
fn override_must_be_declared_for_conformance() {
    // The declaration conformance gate covers the override: registering the
    // override without declaring the global is a shape mismatch.
    struct UndeclaredOverride;
    impl NativeModule for UndeclaredOverride {
        fn name(&self) -> &'static str {
            "undeclared"
        }
        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("")
        }
        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.leaf_function("assert", ModuleBinding::GlobalOverride, |(): ()| 1.0_f64);
        }
    }

    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(UndeclaredOverride))
        .build()
        .expect_err("an undeclared override must fail the conformance gate");
    assert!(
        error.to_string().contains("missing from declaration"),
        "the gate reports the undeclared override: {error}"
    );
}

#[test]
fn hidden_binding_is_host_only_and_contributes_types() {
    let surface = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(HiddenMethodsModule))
        .build()
        .expect("a hidden binding with a type-only declaration validates");

    // The declaration's type alias reaches the checker even though no global
    // is declared, so annotations naming it resolve...
    assert!(
        !strict_has_errors(
            &surface,
            "--!strict\nlocal m = nil :: WidgetMethods?\nreturn m\n"
        ),
        "the hidden module's type alias resolves in strict scripts"
    );
    // ...while the binding name resolves to no global value.
    assert!(
        strict_has_errors(&surface, "--!strict\nreturn widget_methods\n"),
        "the hidden table is not a checker-visible global"
    );

    // Runtime: invisible to the sandboxed script, fetchable by the host.
    let mut vm = deterministic_vm(&surface);
    let chunk = surface
        .compile_bytes(b"return widget_methods == nil")
        .expect("compiles");
    let module = vm.load(&chunk).expect("loads");
    let hidden_from_script = vm
        .call_protected(&module, Default::default())
        .expect("not fatal")
        .expect("not a script error");
    assert_eq!(
        format!("{hidden_from_script:?}"),
        "[Boolean(true)]",
        "the hidden table is not a script-visible global"
    );
    let ping: f64 = vm
        .step(|scope| {
            let table = scope
                .named_get(b"widget_methods")
                .ok_or_else(|| RuntimeError::runtime("hidden table missing"))?;
            let ping: ruau::vm::Function<'_> = table.get(scope, "ping")?;
            scope.call(ping, ())
        })
        .expect("the host fetches and calls the hidden method");
    assert!((ping - 7.0).abs() < f64::EPSILON);
}

#[test]
fn support_chunks_install_hidden_named_registry_values() {
    struct SupportModule;

    impl NativeModule for SupportModule {
        fn name(&self) -> &'static str {
            "support"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.support_chunk(
                "support.proxy",
                b"return { answer = 42, nested = { ok = true } }",
            );
        }
    }

    let surface = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(SupportModule))
        .build()
        .expect("a support chunk has no declaration obligation");
    let mut vm = deterministic_vm(&surface);

    let answer: f64 = vm
        .step(|scope| {
            let table = scope
                .named_get(b"support.proxy")
                .ok_or_else(|| RuntimeError::runtime("support chunk missing"))?;
            table.get(scope, "answer")
        })
        .expect("support chunk return is rooted in the named registry");
    assert_eq!(answer, 42.0);

    let chunk = surface
        .compile_bytes(b"return support == nil")
        .expect("compiles");
    let module = vm.load(&chunk).expect("loads");
    let hidden_from_script = vm
        .call_protected(&module, Default::default())
        .expect("not fatal")
        .expect("not a script error");
    assert_eq!(
        format!("{hidden_from_script:?}"),
        "[Boolean(true)]",
        "support chunks are not script-visible globals"
    );

    vm.clear_named_registry();
    let restored: bool = vm
        .step(|scope| Ok(scope.named_get(b"support.proxy").is_some()))
        .expect("clear_named_registry keeps build-time support values");
    assert!(restored);
}

#[test]
fn support_chunks_use_the_injected_runtime_compiler() {
    struct SupportModule;

    impl NativeModule for SupportModule {
        fn name(&self) -> &'static str {
            "support"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.support_chunk("support.proxy", b"host compiler input");
        }
    }

    struct ObserveCompiler {
        sources: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl ruau_vm::RuntimeCompiler for ObserveCompiler {
        fn compile(
            &self,
            source: &[u8],
            _context: ruau_vm::RuntimeCompileContext,
        ) -> Result<ruau::bytecode::BytecodeChunk, Vec<u8>> {
            self.sources
                .lock()
                .expect("sources lock")
                .push(source.to_vec());
            RuntimeCapabilities::default()
                .enable_runtime_compilation()
                .compile_source(b"return { answer = 77 }", &CompileOptions::default())
                .map_err(|error| error.to_string().into_bytes())
        }
    }

    let surface = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(SupportModule))
        .build()
        .expect("surface validates");
    let sources = Arc::new(Mutex::new(Vec::new()));
    let mut vm = surface
        .vm_builder(&VmConfig::deterministic(0))
        .runtime_compiler(Arc::new(ObserveCompiler {
            sources: Arc::clone(&sources),
        }))
        .build()
        .expect("VM builds with support chunk");

    let answer: f64 = vm
        .step(|scope| {
            let table = scope
                .named_get(b"support.proxy")
                .ok_or_else(|| RuntimeError::runtime("support chunk missing"))?;
            table.get(scope, "answer")
        })
        .expect("support chunk compiled by injected compiler");
    assert_eq!(answer, 77.0);
    assert_eq!(
        sources.lock().expect("sources lock").as_slice(),
        &[b"host compiler input".to_vec()]
    );
}

#[test]
fn support_chunk_keys_share_the_hidden_binding_namespace() {
    struct SupportFirst;
    struct HiddenSecond;

    impl NativeModule for SupportFirst {
        fn name(&self) -> &'static str {
            "support_first"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.support_chunk("shared", b"return {}");
        }
    }

    impl NativeModule for HiddenSecond {
        fn name(&self) -> &'static str {
            "hidden_second"
        }

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.leaf_function("ping", ModuleBinding::hidden("shared"), |(): ()| 1.0_f64);
        }
    }

    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(SupportFirst))
        .module(Arc::new(HiddenSecond))
        .build()
        .expect_err("support chunks and hidden tables share a named-registry keyspace");
    assert!(
        error.to_string().contains("collides with a support chunk"),
        "the error names the named-registry collision: {error}"
    );
}

#[test]
fn hidden_binding_must_not_be_declared_as_a_global() {
    // The conformance gate covers hidden bindings from the other side: a
    // declaration that declares a global for the hidden table claims a
    // script-visible binding the module never registers.
    struct DeclaredHidden;
    impl NativeModule for DeclaredHidden {
        fn name(&self) -> &'static str {
            "declared_hidden"
        }
        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare widget_methods: { ping: () -> number }")
        }
        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.leaf_function("ping", ModuleBinding::hidden("widget_methods"), |(): ()| {
                7.0_f64
            });
        }
    }

    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(DeclaredHidden))
        .build()
        .expect_err("declaring a global for a hidden binding must fail the gate");
    assert!(
        error.to_string().contains("not registered at runtime"),
        "the gate reports the phantom declared global: {error}"
    );
}

#[test]
fn two_modules_cannot_register_the_same_hidden_member() {
    struct HiddenPing(&'static str);
    impl NativeModule for HiddenPing {
        fn name(&self) -> &'static str {
            self.0
        }
        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("")
        }
        fn build(&self, builder: &mut dyn ModuleBuilder) {
            builder.leaf_function("ping", ModuleBinding::hidden("shared"), |(): ()| 1.0_f64);
        }
    }

    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(Arc::new(HiddenPing("first")))
        .module(Arc::new(HiddenPing("second")))
        .build()
        .expect_err("a duplicate hidden member across modules must fail validation");
    assert!(
        error
            .to_string()
            .contains("duplicate hidden binding shared.ping"),
        "the error names the duplicate hidden member: {error}"
    );
}
