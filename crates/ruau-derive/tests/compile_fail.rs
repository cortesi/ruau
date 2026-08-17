//! Compile-fail coverage for the embedding derive macros' misuse contracts.

#[cfg(test)]
mod tests {
    use ruau_compile_fail::Dependency;

    #[test]
    fn derive_misuse_compile_failures() {
        ruau_compile_fail::run(
            env!("CARGO_MANIFEST_DIR"),
            "ruau-derive",
            &["tests/ui"],
            &[
                Dependency::new("ruau-derive", "."),
                Dependency::new("ruau-vm", "../ruau-vm"),
            ],
        );
    }
}
