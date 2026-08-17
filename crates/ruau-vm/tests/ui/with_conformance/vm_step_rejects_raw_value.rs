use ruau_vm::RawValue;
use ruau_vm::Vm;

fn escapes(vm: &mut Vm) {
    let escaped = vm.step(|_scope| Ok(RawValue::Nil));
    let _ = escaped;
}

fn main() {}
