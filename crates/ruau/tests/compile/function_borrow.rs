use ruau::{Luau, Result};

struct Test(i32);

fn main() {
    let test = Test(0);

    let lua = Luau::new();
    let _ = lua.create_function(|_, ()| -> Result<i32> { Ok(test.0) });
}
