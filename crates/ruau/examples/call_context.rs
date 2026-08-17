//! Apply one complete `CallOptions` context to a borrowed VM step.

use std::sync::{Arc, Mutex};

use ruau::{
    bytecode::{CompileOptions, compile_source},
    vm::{Ambient, CallOptions, Limits, RuntimeCapabilities, Vm},
};

struct RequestName(&'static str);

fn capture(output: &Arc<Mutex<Vec<u8>>>) -> ruau::vm::PrintSink {
    let output = Arc::clone(output);
    Box::new(move |line| output.lock().expect("output lock").extend_from_slice(line))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let default_output = Arc::new(Mutex::new(Vec::new()));
    let call_output = Arc::new(Mutex::new(Vec::new()));
    let mut vm = Vm::builder()
        .ambient(Ambient::deterministic(0))
        .limits(Limits::unlimited())
        .runtime_capabilities(RuntimeCapabilities::default())
        .app_data(RequestName("default"))
        .print_sink(capture(&default_output))
        .trusted_host()
        .build()?;
    let chunk = compile_source("print('request')", &CompileOptions::default(), None)?;
    let module = vm.load(&chunk)?;
    let options = CallOptions::new()
        .app_data(RequestName("one"))
        .print_sink(capture(&call_output));

    vm.step_with(&options, |scope| {
        assert_eq!(
            scope.app_data::<RequestName>().expect("request name").0,
            "one"
        );
        let main = scope.module_function(&module);
        scope.call::<_, ()>(main, ())
    })?;
    vm.step(|scope| {
        assert_eq!(
            scope.app_data::<RequestName>().expect("default name").0,
            "default"
        );
        Ok(())
    })?;

    assert_eq!(
        *call_output.lock().expect("call output"),
        b"request\n".to_vec()
    );
    assert!(default_output.lock().expect("default output").is_empty());
    vm.unload(module);
    Ok(())
}
