use ruau_vm::Vm;
use ruau_vm_api::RawValue;

fn escapes(vm: &mut Vm) {
    let escaped = vm.step(|_scope| Ok(RawValue::Nil));
    let _ = escaped;
}

fn main() {}
