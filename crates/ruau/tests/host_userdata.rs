//! Embedder-typed host userdata through the curated `ruau` surface: checker
//! admission of `declare class` methods via the host-module declaration path,
//! and an end-to-end sandboxed script exercising a registered type.
#![allow(clippy::tests_outside_test_module)]

use std::sync::{Arc, Mutex};

use ruau::{
    analysis::AnalysisMode,
    surface::{Surface, VmConfig},
    typecheck::{Config, Diagnostics},
    vm::{
        Ambient, CallOptions, FromLuaMulti, HostType, HostTypeBuilder, Limits, MarshaledPair,
        MarshaledValue, ModuleBuilderExt, MultiValue, RuntimeError, Scope, ScopedHostFunction,
        ScopedValue,
    },
    vm_api::{ModuleBinding, ModuleBuilder, NativeModule},
};
use serde_json::json;

/// The class half of the `.d.luau` surface, carried by the [`HostType`] and
/// spliced into the module declaration. A `declare class` creates type
/// bindings only, so the declaration-vs-binding audit sees exactly the
/// `counter.make` binding.
const COUNTER_CLASS: &str = "\
declare class Counter
    function get(self): number
    function add(self, by: number): number
end";

/// The binding half: the host function that hands instances out.
const COUNTER_GLOBALS: &str = "\
declare counter: {
    make: (number) -> Counter,
}";

/// The embedded host value behind `Counter` userdata.
struct Counter {
    count: f64,
}

fn counter_get(_: &Scope<'_>, counter: &Counter, (): ()) -> Result<f64, RuntimeError> {
    Ok(counter.count)
}

fn counter_add(_: &Scope<'_>, counter: &mut Counter, by: f64) -> Result<f64, RuntimeError> {
    counter.count += by;
    Ok(counter.count)
}

fn counter_type() -> HostType {
    HostTypeBuilder::<Counter>::new("Counter")
        .method("get", counter_get)
        .method_mut("add", counter_add)
        .marshal(|counter| {
            MarshaledValue::Table(vec![MarshaledPair {
                key: MarshaledValue::String(b"count".to_vec()),
                value: MarshaledValue::Number(counter.count),
            }])
        })
        .tostring(|counter| format!("Counter({})", counter.count))
        .declaration(COUNTER_CLASS)
        .build()
}

/// `counter.make(start)`: hands a fresh instance to the script. Returning a
/// scope-borrowed `Userdata` needs a direct [`ScopedHostFunction`] impl (the
/// `scoped_host_fn` adapter is for owned argument/return shapes).
struct MakeCounter;

impl ScopedHostFunction for MakeCounter {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let start = f64::from_lua_multi(args, scope)?;
        let userdata = scope.create_userdata(Counter { count: start })?;
        Ok(MultiValue::from_values(vec![ScopedValue::Userdata(
            userdata,
        )]))
    }
}

struct CounterModule {
    /// The composed `.d.luau`: the host type's class snippet spliced ahead of
    /// the module's own binding declarations.
    declaration: String,
}

impl CounterModule {
    fn new() -> Self {
        let class = counter_type();
        Self {
            declaration: format!(
                "{}\n\n{}\n",
                class
                    .declaration()
                    .expect("the Counter type carries a class"),
                COUNTER_GLOBALS
            ),
        }
    }
}

impl NativeModule for CounterModule {
    fn name(&self) -> &str {
        "counter"
    }

    fn declaration(&self) -> ruau_decl::DeclSource<'_> {
        ruau_decl::DeclSource::Text(&self.declaration)
    }

    fn build(&self, builder: &mut dyn ModuleBuilder) {
        ModuleBuilderExt::host_type(builder, counter_type());
        builder.scoped_function(
            "make",
            ModuleBinding::library("counter"),
            Box::new(MakeCounter),
        );
    }
}

fn counter_surface() -> Surface {
    Surface::builder()
        .module(Arc::new(CounterModule::new()))
        .build()
        .expect("the counter surface validates")
}

fn check(surface: &Surface, source: &str, mode: AnalysisMode) -> Diagnostics {
    let mut checker = surface.new_checker();
    checker
        .check_source_with_config(source, Config::with_source_mode(mode))
        .diagnostics()
        .clone()
}

#[test]
fn declared_userdata_methods_typecheck_via_the_module_declaration() {
    let surface = counter_surface();

    // Well-typed method calls on the declared class are admitted.
    let clean = check(
        &surface,
        "local c = counter.make(5)\n\
         local total: number = c:add(2)\n\
         local read: number = c:get()\n\
         local _ = total + read\n",
        AnalysisMode::Strict,
    );
    assert!(clean.is_empty(), "unexpected diagnostics: {clean:?}");

    // A wrong argument type to a declared method is rejected.
    let bad_argument = check(
        &surface,
        "local c = counter.make(5)\nlocal _ = c:add('nope')\n",
        AnalysisMode::Strict,
    );
    assert!(
        !bad_argument.is_empty(),
        "c:add('nope') must fail the checker"
    );

    // A method the class does not declare is rejected.
    let unknown_method = check(
        &surface,
        "local c = counter.make(5)\nc:missing()\n",
        AnalysisMode::Strict,
    );
    assert!(
        !unknown_method.is_empty(),
        "c:missing() must fail the checker"
    );
}

#[test]
fn surface_conformance_check_uses_the_retained_surface() {
    let surface = counter_surface();
    let check = surface.check_conformance(
        "--!strict\nreturn { count = 7, extra = true }",
        "--!strict\ndeclare module: { count: number }",
    );
    assert!(check.is_ok(), "{:?}", check.diagnostics());

    let bad = surface.check_conformance(
        "--!strict\nreturn { count = \"bad\" }",
        "--!strict\ndeclare module: { count: number }",
    );
    assert!(!bad.diagnostics().is_empty(), "{bad:?}");
}

#[tokio::test]
async fn a_sandboxed_script_exercises_a_registered_type_end_to_end() {
    let surface = counter_surface();
    let source = "--!nonstrict\n\
         local c = counter.make(3)\n\
         c:add(4)\n\
         -- The shared metatable is protected: scripts see the type name, and\n\
         -- cannot reach (or mutate) the dispatch table behind it.\n\
         local meta = getmetatable(c)\n\
         local ok = pcall(function() (meta :: any).__index = nil end)\n\
         assert(not ok, 'metatable must not be writable')\n\
         print(c)\n\
         return c:get(), typeof(c), meta, tostring(c), c\n";

    // Conformance-style admission: the script checks against the surface
    // before it is compiled for the surface's runtime capabilities and run sandboxed.
    let diagnostics = check(&surface, source, AnalysisMode::Nonstrict);
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );

    let mut vm = surface
        .vm_builder(&VmConfig::untrusted(
            Ambient::deterministic(7),
            Limits {
                gas: Some(1_000_000),
                max_memory_bytes: Some(8 * 1024 * 1024),
                ..Limits::unlimited()
            },
        ))
        .build()
        .expect("sandboxed VM builds");
    let chunk = surface.compile_bytes(source.as_bytes()).expect("compile");
    let module = vm.load_named(&chunk, b"=counter_e2e.luau").expect("load");
    let print_bytes = Arc::new(Mutex::new(Vec::new()));
    let print_capture = Arc::clone(&print_bytes);
    let values = vm
        .exec_async(
            &module,
            CallOptions::new().print_sink(Box::new(move |bytes| {
                print_capture
                    .lock()
                    .expect("print capture lock is not poisoned")
                    .extend_from_slice(bytes);
            })),
        )
        .await
        .expect("script runs");
    match values.as_slice() {
        [
            MarshaledValue::Number(total),
            MarshaledValue::String(type_name),
            MarshaledValue::String(meta),
            MarshaledValue::String(rendered),
            counter,
        ] => {
            assert_eq!(*total, 7.0);
            assert_eq!(type_name.as_slice(), b"Counter");
            assert_eq!(meta.as_slice(), b"Counter");
            assert_eq!(rendered.as_slice(), b"Counter(7)");
            assert_eq!(
                ruau::vm::serde::marshaled_to_json(counter).expect("userdata hook is JSON"),
                json!({ "count": 7.0 })
            );
            assert_eq!(
                String::from_utf8(
                    print_bytes
                        .lock()
                        .expect("print capture lock is not poisoned")
                        .clone()
                )
                .expect("print output is utf-8"),
                "Counter(7)\n"
            );
        }
        other => panic!("unexpected result shape: {other:?}"),
    }
}
