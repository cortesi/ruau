//! Drive a retained VM directly with typed root and callback handles.

use std::task::{Context, Poll, Waker};

use ruau::{
    bytecode::CompileOptions,
    session::{LoadTarget, Runtime},
    source::{ModuleId, Source},
    surface::{Surface, VmConfig},
    vm::{CallOptions, FromLuaMulti, Function, Limits, ScopedValue},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let surface = Surface::new();
    let chunk = surface.compile(
        &Source::text(
            ModuleId::canonicalized("retained-runtime"),
            "return function(value) return value + 1 end",
        ),
        &CompileOptions::default(),
    )?;
    let mut runtime = Runtime::new(
        surface,
        &VmConfig::untrusted(
            ruau::vm::Ambient::deterministic(7),
            ruau::vm::Limits::unlimited(),
        ),
    )?;
    let root = runtime.load_compiled(&chunk, &LoadTarget::named("retained-runtime.luau"))?;

    let callback = runtime.step_root(&root, &CallOptions::new(), |scope, main| {
        let callback: Function<'_> = scope.call(main, ())?;
        scope.stash_function(callback)
    })?;
    let callback = runtime.retain(callback);
    let argument = runtime.step(&CallOptions::new(), |scope| {
        scope.stash_value(ScopedValue::Number(41.0))
    })?;
    let argument = runtime.retain(argument);
    let domain = runtime.create_module_domain();
    let invocation = runtime.create_function_invocation(domain, &callback, vec![argument])?;
    runtime.release(&callback)?;

    let waker = Waker::noop();
    let mut task_context = Context::from_waker(waker);
    let poll_options = CallOptions::new().limits(Limits {
        gas: Some(1_000_000),
        ..Limits::unlimited()
    });
    let result = loop {
        let step = runtime.poll_invocation_with_context_and_result(
            invocation,
            &mut (),
            &poll_options,
            &mut task_context,
            |scope, values| f64::from_lua_multi(values, scope),
        );
        println!("poll used {} gas", step.usage.gas_spent);
        match step.poll {
            Poll::Pending => std::thread::yield_now(),
            Poll::Ready(result) => break result?,
        }
    };
    println!("callback returned {result}");

    runtime.release_module_domain(domain)?;
    runtime.unload(&root)?;
    Ok(())
}
