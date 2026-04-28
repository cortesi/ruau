use std::panic::catch_unwind;

use ruau::Lua;

fn main() {
    let lua = Lua::new();
    catch_unwind(|| lua.create_table().unwrap());
}
