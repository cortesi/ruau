//! Compile-fail coverage for public VM embedding API contracts.

#[cfg(test)]
mod tests {
    use ruau_compile_fail::Dependency;

    #[test]
    fn public_api_compile_failures() {
        let (profile, feature_dirs, features): (&str, &[&str], &[&str]) =
            if cfg!(feature = "conformance") {
                (
                    "ruau-vm-conformance",
                    &["tests/ui", "tests/ui/with_conformance"],
                    &["conformance"],
                )
            } else {
                (
                    "ruau-vm-default",
                    &["tests/ui", "tests/ui/without_conformance"],
                    &[],
                )
            };
        ruau_compile_fail::run(
            env!("CARGO_MANIFEST_DIR"),
            profile,
            feature_dirs,
            &[Dependency::new("ruau-vm", ".")
                .without_default_features()
                .with_features(features)],
        );
    }
}
