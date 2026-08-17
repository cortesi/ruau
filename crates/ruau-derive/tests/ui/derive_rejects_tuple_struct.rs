//! Tuple structs have no stable embedding contract.

use ruau_derive::IntoLua;

#[derive(IntoLua)]
struct Point(i32, i32);

fn main() {}
