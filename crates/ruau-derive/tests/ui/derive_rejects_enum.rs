//! Enums have no stable embedding contract.

use ruau_derive::FromLua;

#[derive(FromLua)]
enum Shape {
    Circle,
    Square,
}

fn main() {}
