cfg_if::cfg_if! {
    if #[cfg(feature = "luau")] {
        include!("main_inner.rs");
    } else {
        fn main() {
            compile_error!("The `luau` feature is required");
        }
    }
}
