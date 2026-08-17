//! Only `rename = "..."` is a valid field attribute.

use ruau_derive::FromLua;

#[derive(FromLua)]
struct Widget {
    #[ruau(skip)]
    name: String,
}

fn main() {}
