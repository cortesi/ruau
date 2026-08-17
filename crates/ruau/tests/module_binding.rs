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

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use ruau::{
    bytecode::CompileOptions,
    module::{Binding, Builder as NativeModuleBuilder},
    source::{ModuleId, Source},
    surface::{Surface, VmConfig},
    vm::{
        Ambient, Limits, ModuleBinding, ModuleExport, ModuleSetupPhase, NativeModule,
        RuntimeCapabilities, RuntimeError, Table, VmBuildError,
        module::{Installer as ModuleBuilder, InstallerExt as ModuleBuilderExt},
    },
};

fn compile_source(surface: &Surface, source: &[u8]) -> ruau::bytecode::BytecodeChunk {
    surface
        .compile(
            &Source::bytes(ModuleId::canonicalized("module-binding"), source.to_vec()),
            &CompileOptions::default(),
        )
        .expect("compiles")
}

fn vm_config() -> VmConfig {
    VmConfig::untrusted(Ambient::deterministic(0), Limits::unlimited())
}

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

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text(
            "declare function assert(value: boolean, message: string?): ()",
        )
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
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

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("type WidgetMethods = { ping: () -> number }")
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
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

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("declare native: { answer: () -> number }")
    }

    fn export(&self) -> ModuleExport {
        self.export
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        builder.leaf_function("answer", ModuleBinding::library("native"), |(): ()| {
            42.0_f64
        });
    }
}

fn deterministic_vm(surface: &Surface) -> ruau::vm::Vm {
    surface
        .vm_builder(&vm_config())
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
        ruau::vm::RuntimeErrorKind::UnresolvedRequire,
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

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text({
                "declare native: { answer: () -> number }\ndeclare leaked: number"
            })
        }

        fn export(&self) -> ModuleExport {
            ModuleExport::Require
        }

        fn install(&self, builder: &mut dyn ModuleBuilder) {
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
        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("declare function no_such_builtin(): number")
        }
        fn install(&self, builder: &mut dyn ModuleBuilder) {
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
    let chunk = compile_source(&surface, b"return assert()");
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
        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("")
        }
        fn install(&self, builder: &mut dyn ModuleBuilder) {
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
    let chunk = compile_source(&surface, b"return widget_methods == nil");
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

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("")
        }

        fn install(&self, builder: &mut dyn ModuleBuilder) {
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

    let chunk = compile_source(&surface, b"return support == nil");
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

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("")
        }

        fn install(&self, builder: &mut dyn ModuleBuilder) {
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
        .vm_builder(&vm_config())
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

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("")
        }

        fn install(&self, builder: &mut dyn ModuleBuilder) {
            builder.support_chunk("shared", b"return {}");
        }
    }

    impl NativeModule for HiddenSecond {
        fn name(&self) -> &'static str {
            "hidden_second"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("")
        }

        fn install(&self, builder: &mut dyn ModuleBuilder) {
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
        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text(
                "declare widget_methods: { ping: () -> number }",
            )
        }
        fn install(&self, builder: &mut dyn ModuleBuilder) {
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
        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("")
        }
        fn install(&self, builder: &mut dyn ModuleBuilder) {
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

fn private_input_provider() -> Arc<dyn NativeModule> {
    let mut builder = NativeModuleBuilder::new("private-input-provider");
    builder.leaf_function("label", Binding::hidden("private.first"), |(): ()| "first");
    builder.leaf_function("value", Binding::hidden("private.first"), |(): ()| 4.0_f64);
    builder.leaf_function(
        "label",
        Binding::hidden("private.second"),
        |(): ()| "second",
    );
    builder.leaf_function("value", Binding::hidden("private.second"), |(): ()| 6.0_f64);
    builder.build().expect("private-input provider validates")
}

fn private_input_consumer(
    source: impl Into<Vec<u8>>,
    private_inputs: impl IntoIterator<Item = &'static str>,
) -> Arc<dyn NativeModule> {
    let mut builder = NativeModuleBuilder::new("private-input-consumer");
    builder.source_value_with(
        "private_api",
        Binding::hidden("private.proof"),
        source,
        private_inputs,
    );
    builder.build().expect("private-input consumer validates")
}

fn private_input_surface() -> Surface {
    let consumer = private_input_consumer(
        b"local first, second = ...\nreturn { order = first.label() .. ':' .. second.label(), total = first.value() + second.value(), first = first, second = second }".to_vec(),
        ["private.first", "private.second"],
    );
    Surface::builder()
        .enable_runtime_compilation()
        .module(consumer)
        .module(private_input_provider())
        .build()
        .expect("a consumer registered before its providers validates")
}

fn private_input_ids(vm: &mut ruau::vm::Vm) -> (ruau::vm::TableId, ruau::vm::TableId) {
    vm.step(|scope| {
        let first = scope
            .named_get(b"private.first")
            .ok_or_else(|| RuntimeError::runtime("first private table missing"))?;
        let second = scope
            .named_get(b"private.second")
            .ok_or_else(|| RuntimeError::runtime("second private table missing"))?;
        Ok((first.id(), second.id()))
    })
    .expect("private table ids are available")
}

struct SupportProvider;

impl NativeModule for SupportProvider {
    fn name(&self) -> &str {
        "support-provider"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("")
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        builder.support_chunk("private.support", b"return {}");
    }
}

#[test]
fn source_value_private_inputs_preserve_order_identity_and_host_functions() {
    let surface = private_input_surface();
    let mut vm = deterministic_vm(&surface);
    let (first_id, second_id) = private_input_ids(&mut vm);

    let (order, total, returned_first, returned_second): (
        String,
        f64,
        ruau::vm::TableId,
        ruau::vm::TableId,
    ) = vm
        .step(|scope| {
            let proof = scope
                .named_get(b"private.proof")
                .ok_or_else(|| RuntimeError::runtime("private proof table missing"))?;
            let public: Table<'_> = proof.get(scope, "private_api")?;
            let order: String = public.get(scope, "order")?;
            let total: f64 = public.get(scope, "total")?;
            let first: Table<'_> = public.get(scope, "first")?;
            let second: Table<'_> = public.get(scope, "second")?;
            Ok((order, total, first.id(), second.id()))
        })
        .expect("source value exposes its proof fields");

    assert_eq!(order, "first:second");
    assert!((total - 10.0).abs() < f64::EPSILON);
    assert_eq!(returned_first, first_id);
    assert_eq!(returned_second, second_id);

    vm.clear_named_registry();
    assert_eq!(private_input_ids(&mut vm), (first_id, second_id));

    let mut other_vm = deterministic_vm(&surface);
    let other_ids = private_input_ids(&mut other_vm);
    assert_ne!(other_ids.0, first_id);
    assert_ne!(other_ids.1, second_id);
}

#[test]
fn surface_audit_rejects_invalid_private_input_providers() {
    let missing = private_input_consumer(b"return {}".to_vec(), ["missing"]);
    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(missing)
        .build()
        .expect_err("a missing hidden-table provider must fail the surface audit");
    assert!(
        error
            .to_string()
            .contains("missing hidden module table missing"),
        "the audit identifies the missing provider: {error}"
    );

    let support = private_input_consumer(b"return {}".to_vec(), ["private.support"]);
    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(support)
        .module(Arc::new(SupportProvider))
        .build()
        .expect_err("a support-chunk result cannot provide a private input");
    assert!(
        error
            .to_string()
            .contains("names support chunk private.support"),
        "the audit identifies the invalid support-chunk provider: {error}"
    );

    let mut source_provider = NativeModuleBuilder::new("source-provider");
    source_provider.leaf_function("ping", Binding::hidden("private.source"), |(): ()| ());
    source_provider.source_value(
        "late",
        Binding::hidden("private.source"),
        b"return 1".to_vec(),
    );
    let source_provider = source_provider
        .build()
        .expect("the mixed hidden table is a valid module on its own");
    let source = private_input_consumer(b"return {}".to_vec(), ["private.source"]);
    let error = Surface::builder()
        .enable_runtime_compilation()
        .module(source)
        .module(source_provider)
        .build()
        .expect_err("a source-populated hidden table cannot provide a private input");
    assert!(
        error
            .to_string()
            .contains("hidden table private.source that is also populated by a source value"),
        "the audit identifies the invalid source-value dependency: {error}"
    );
}

fn direct_vm_build(module: Arc<dyn NativeModule>) -> Result<ruau::vm::Vm, VmBuildError> {
    ruau::vm::Vm::builder()
        .ambient(Ambient::deterministic(0))
        .limits(Limits::unlimited())
        .runtime_capabilities(RuntimeCapabilities::default().enable_runtime_compilation())
        .module(module)
        .sandboxed()
        .build()
}

struct LowLevelPrivateInputs {
    inputs: Vec<&'static str>,
}

impl NativeModule for LowLevelPrivateInputs {
    fn name(&self) -> &str {
        "low-level-private-inputs"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("")
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        builder.source_value_with("value", ModuleBinding::Global, b"return 1", &self.inputs);
    }
}

#[test]
fn low_level_source_value_private_input_keys_are_validated() {
    let mut no_inputs = direct_vm_build(Arc::new(LowLevelPrivateInputs { inputs: Vec::new() }))
        .expect("an empty low-level private-input list matches source_value");
    assert_eq!(
        run_vm_source(&mut no_inputs, b"return value"),
        "[Number(1.0)]"
    );

    let empty = direct_vm_build(Arc::new(LowLevelPrivateInputs { inputs: vec![""] }));
    let Err(VmBuildError::ModuleInstall(error)) = empty else {
        panic!("an empty low-level private-input key must fail module installation");
    };
    assert!(
        error
            .to_string()
            .contains("empty private input at position 1")
    );

    let duplicate = direct_vm_build(Arc::new(LowLevelPrivateInputs {
        inputs: vec!["kernel", "kernel"],
    }));
    let Err(VmBuildError::ModuleInstall(error)) = duplicate else {
        panic!("a repeated low-level private-input key must fail module installation");
    };
    assert!(
        error
            .to_string()
            .contains("repeats private input `kernel` at position 2")
    );
}

struct LowLevelNamedCollision;

impl NativeModule for LowLevelNamedCollision {
    fn name(&self) -> &str {
        "low-level-named-collision"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("")
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        builder.support_chunk("shared", b"return {}");
        builder.leaf_function("ping", ModuleBinding::hidden("shared"), |(): ()| ());
    }
}

#[test]
fn low_level_support_and_hidden_keys_cannot_collide() {
    let result = direct_vm_build(Arc::new(LowLevelNamedCollision));
    let Err(VmBuildError::ModuleInstall(error)) = result else {
        panic!("a low-level named-registry collision must fail module installation");
    };
    assert!(
        error
            .to_string()
            .contains("collides with named-registry key `shared`")
    );
}

struct LowLevelSupportInput;

impl NativeModule for LowLevelSupportInput {
    fn name(&self) -> &str {
        "low-level-support-input"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("")
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        builder.source_value_with(
            "value",
            ModuleBinding::Global,
            b"return 1",
            &["support.provider"],
        );
        builder.support_chunk("support.provider", b"return {}");
    }
}

#[test]
fn low_level_support_chunk_results_cannot_provide_private_inputs() {
    let result = direct_vm_build(Arc::new(LowLevelSupportInput));
    let error = expect_setup_phase(result, ModuleSetupPhase::PrivateInput);
    assert_eq!(error.private_input_key(), Some("support.provider"));
    assert!(error.diagnostic().contains("belongs to a support chunk"));
}

struct LowLevelSourceInput;

impl NativeModule for LowLevelSourceInput {
    fn name(&self) -> &str {
        "low-level-source-input"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("")
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        builder.leaf_function(
            "ping",
            ModuleBinding::hidden("source.provider"),
            |(): ()| (),
        );
        builder.source_value(
            "late",
            ModuleBinding::hidden("source.provider"),
            b"return 1",
        );
        builder.source_value_with(
            "value",
            ModuleBinding::Global,
            b"return 1",
            &["source.provider"],
        );
    }
}

#[test]
fn low_level_source_results_cannot_populate_private_input_tables() {
    let result = direct_vm_build(Arc::new(LowLevelSourceInput));
    let error = expect_setup_phase(result, ModuleSetupPhase::PrivateInput);
    assert_eq!(error.private_input_key(), Some("source.provider"));
    assert!(
        error
            .diagnostic()
            .contains("also populated by a source value")
    );
}

struct ResolveBeforeRun {
    calls: Arc<AtomicUsize>,
}

impl NativeModule for ResolveBeforeRun {
    fn name(&self) -> &str {
        "resolve-before-run"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("")
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        let calls = Arc::clone(&self.calls);
        builder.leaf_function("mark", ModuleBinding::Global, move |(): ()| {
            calls.fetch_add(1, Ordering::SeqCst);
        });
        builder.leaf_function("ping", ModuleBinding::hidden("present"), |(): ()| ());
        builder.source_value_with(
            "first",
            ModuleBinding::Global,
            b"mark(); return {}",
            &["present"],
        );
        builder.source_value_with(
            "second",
            ModuleBinding::Global,
            b"mark(); return {}",
            &["missing"],
        );
    }
}

#[test]
fn every_private_input_resolves_before_any_trusted_source_runs() {
    let calls = Arc::new(AtomicUsize::new(0));
    let result = direct_vm_build(Arc::new(ResolveBeforeRun {
        calls: Arc::clone(&calls),
    }));
    let Err(VmBuildError::ModuleSetup(error)) = result else {
        panic!("the missing input must fail VM construction");
    };
    assert_eq!(error.phase(), ModuleSetupPhase::PrivateInput);
    assert_eq!(error.module(), "resolve-before-run");
    assert_eq!(error.source_name(), "second");
    assert_eq!(error.private_input_index(), Some(0));
    assert_eq!(error.private_input_key(), Some("missing"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

struct SourceFailureModule {
    member: &'static str,
    binding: ModuleBinding,
    source: &'static [u8],
    support_chunk: bool,
}

struct ErrorChunkCompiler;

impl ruau::vm::RuntimeCompiler for ErrorChunkCompiler {
    fn compile(
        &self,
        _source: &[u8],
        _context: ruau::vm::RuntimeCompileContext,
    ) -> Result<ruau::bytecode::BytecodeChunk, Vec<u8>> {
        Ok(ruau::bytecode::BytecodeChunk::Error {
            message: b"compiler returned an error chunk".to_vec(),
        })
    }
}

impl NativeModule for SourceFailureModule {
    fn name(&self) -> &str {
        "failure"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("")
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        if self.support_chunk {
            builder.support_chunk(self.member, self.source);
        } else {
            builder.source_value(self.member, self.binding.clone(), self.source);
        }
    }
}

fn source_failure(
    member: &'static str,
    binding: ModuleBinding,
    source: &'static [u8],
) -> Arc<dyn NativeModule> {
    Arc::new(SourceFailureModule {
        member,
        binding,
        source,
        support_chunk: false,
    })
}

fn expect_setup_phase(
    result: Result<ruau::vm::Vm, VmBuildError>,
    phase: ModuleSetupPhase,
) -> ruau::vm::ModuleSetupError {
    let Err(VmBuildError::ModuleSetup(error)) = result else {
        panic!("trusted source setup must return VmBuildError::ModuleSetup");
    };
    assert_eq!(error.phase(), phase, "{error}");
    error
}

#[test]
fn trusted_source_failures_are_direct_structured_vm_build_errors() {
    let compile = expect_setup_phase(
        direct_vm_build(Arc::new(SourceFailureModule {
            member: "broken-support",
            binding: ModuleBinding::Global,
            source: b"local =",
            support_chunk: true,
        })),
        ModuleSetupPhase::Compile,
    );
    assert_eq!(compile.source_name(), "broken-support");
    assert!(!compile.diagnostic().is_empty());

    let load = ruau::vm::Vm::builder()
        .ambient(Ambient::deterministic(0))
        .limits(Limits::unlimited())
        .runtime_capabilities(RuntimeCapabilities::default().enable_runtime_compilation())
        .runtime_compiler(Arc::new(ErrorChunkCompiler))
        .module(source_failure("value", ModuleBinding::Global, b"ignored"))
        .sandboxed()
        .build();
    expect_setup_phase(load, ModuleSetupPhase::Load);

    let execute = expect_setup_phase(
        direct_vm_build(source_failure(
            "value",
            ModuleBinding::Global,
            b"\nerror('boom')",
        )),
        ModuleSetupPhase::Execute,
    );
    assert_eq!(execute.module(), "failure");
    assert_eq!(execute.source_name(), "value");
    assert!(execute.diagnostic().contains("boom"), "{execute}");
    assert!(
        execute.diagnostic().contains(":2:"),
        "the runtime diagnostic retains the source line: {execute}"
    );

    expect_setup_phase(
        direct_vm_build(source_failure(
            "value",
            ModuleBinding::Global,
            b"return 1, 2",
        )),
        ModuleSetupPhase::ResultCount,
    );
    expect_setup_phase(
        direct_vm_build(source_failure("print", ModuleBinding::Global, b"return 1")),
        ModuleSetupPhase::Install,
    );
}

#[test]
fn private_kernel_builds_a_frozen_public_value_without_registry_visibility() {
    let mut provider = NativeModuleBuilder::new("kernel-provider");
    provider.leaf_function("answer", Binding::hidden("eguidev.kernel"), |(): ()| 42_i64);
    let provider = provider.build().expect("kernel provider validates");

    let mut consumer = NativeModuleBuilder::from_declaration(
        "eguidev-consumer",
        ruau_declaration::DeclarationSource::Text("declare eguidev: { answer: number }"),
    );
    consumer.source_value_with(
        "eguidev",
        Binding::declared_global(),
        b"local kernel = ...\nreturn table.freeze({ answer = kernel.answer() })".to_vec(),
        ["eguidev.kernel"],
    );
    let consumer = consumer.build().expect("kernel consumer validates");

    let surface = Surface::builder()
        .enable_runtime_compilation()
        .module(consumer)
        .module(provider)
        .build()
        .expect("the private kernel satisfies the declared public source value");
    let checked = surface
        .new_checker()
        .check_source("--!strict\nlocal answer: number = eguidev.answer\nreturn answer\n");
    assert!(
        !checked.has_errors(),
        "{}",
        checked.diagnostics().render("kernel-consumer.luau")
    );

    let mut vm = surface
        .vm_builder(&vm_config())
        .module_source(Arc::new(ruau::source::InMemorySource::new()))
        .build()
        .expect("kernel surface builds with require enabled");
    let chunk = compile_source(
        &surface,
        b"return eguidev.answer, (_G == nil or _G['eguidev.kernel'] == nil), named_get == nil",
    );
    let loaded = vm.load(&chunk).expect("sandbox probe loads");
    let values = vm
        .call_protected(&loaded, Default::default())
        .expect("sandbox probe is not fatal")
        .expect("sandbox probe succeeds");
    assert_eq!(
        format!("{values:?}"),
        "[Integer(42), Boolean(true), Boolean(true)]"
    );

    let require_chunk = compile_source(&surface, b"return require('eguidev.kernel')");
    let require_loaded = vm.load(&require_chunk).expect("require probe loads");
    let require_error = vm
        .call_protected(&require_loaded, Default::default())
        .expect("the missing private key is an ordinary script failure")
        .expect_err("the private kernel is not a require module");
    assert_eq!(
        require_error.kind(),
        ruau::vm::RuntimeErrorKind::UnresolvedRequire
    );
}
