#![allow(missing_docs, clippy::missing_docs_in_private_items)]

cfg_if::cfg_if! {
    if #[cfg(feature = "luau")] {
        include!("main_inner.rs");
    } else {
        fn main() {
            compile_error!("The `luau` feature is required");
        }
    }
}
