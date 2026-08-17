use ruau_vm::Vm;

struct Context(u32);

fn escapes(vm: &mut Vm, context: &mut Context) {
    let escaped = vm.step_with_context(context, &Default::default(), |scope| {
        Ok(scope.context_mut::<Context>().expect("context"))
    });
    let _ = escaped;
}

fn main() {}
