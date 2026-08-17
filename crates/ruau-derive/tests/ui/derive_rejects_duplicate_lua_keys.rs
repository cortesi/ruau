use ruau_derive::IntoLua;

#[derive(IntoLua)]
struct DuplicateKeys {
    #[ruau(rename = "value")]
    renamed: i64,
    value: i64,
}

fn main() {}
