use ruau_vm::Vm;

fn escapes(vm: &mut Vm) {
    let escaped = vm.step(|scope| scope.create_table());
    let _ = escaped;
}

fn main() {}
