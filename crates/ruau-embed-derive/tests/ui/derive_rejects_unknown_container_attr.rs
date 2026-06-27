//! Only `crate = "..."` is a valid container attribute.

use ruau_embed_derive::IntoLua;

#[derive(IntoLua)]
#[ruau(rename = "widget")]
struct Widget {
    name: String,
}

fn main() {}
