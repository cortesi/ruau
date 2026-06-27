//! Compile-fail coverage for public VM embedding API contracts.

#[cfg(test)]
mod tests {
    #[test]
    fn public_api_compile_failures() {
        let cases = trybuild::TestCases::new();
        cases.compile_fail("tests/ui/*.rs");
    }
}
