use std::panic::catch_unwind;

use ruau::Luau;

fn main() {
    let lua = Luau::new();
    let table = lua.create_table().unwrap();
    catch_unwind(move || table.set("a", "b").unwrap());
}
