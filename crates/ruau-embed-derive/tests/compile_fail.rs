//! Compile-fail coverage for the embedding derive macros' misuse contracts.

#[cfg(test)]
mod tests {
    #[test]
    fn derive_misuse_compile_failures() {
        let cases = trybuild::TestCases::new();
        cases.compile_fail("tests/ui/*.rs");
    }
}
