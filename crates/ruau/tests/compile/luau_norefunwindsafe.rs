use std::panic::catch_unwind;

use ruau::Luau;

fn main() {
    let lua = Luau::new();
    catch_unwind(|| lua.create_table().unwrap());
}
