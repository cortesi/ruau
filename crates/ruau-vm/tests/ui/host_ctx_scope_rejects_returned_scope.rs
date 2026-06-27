use std::marker::PhantomData;

use ruau_vm::{AsyncHostContext, Scope};

struct Borrowed<'scope>(PhantomData<&'scope ()>);

fn borrowed<'scope>(_: &'scope Scope<'scope>) -> Borrowed<'scope> {
    Borrowed(PhantomData)
}

fn rejects_scope(ctx: AsyncHostContext) {
    let _future = ctx.scope(|scope| Ok(borrowed(scope)));
}

fn main() {}
