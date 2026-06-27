use ruau_vm::Vm;

struct Token(u32);

fn escapes(vm: &mut Vm) {
    let escaped = vm.step(|scope| {
        let userdata = scope.create_userdata(Token(7))?;
        userdata.borrow::<Token>(scope)
    });
    let _ = escaped;
}

fn main() {}
