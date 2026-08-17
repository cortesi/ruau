//! Only `crate = "..."` is a valid container attribute.

use ruau_derive::IntoLua;

#[derive(IntoLua)]
#[ruau(rename = "widget")]
struct Widget {
    name: String,
}

fn main() {}
