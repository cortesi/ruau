//! compile integration tests.

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    const COMPILE_FAIL_FIXTURES: &[&str] = &[
        "tests/compile/checked_module_private.rs",
        "tests/compile/function_borrow.rs",
        "tests/compile/internal_state_private.rs",
        "tests/compile/luau_norefunwindsafe.rs",
        "tests/compile/raw_internals_hidden.rs",
        "tests/compile/runtime_require_private.rs",
        "tests/compile/resolver_internals_private.rs",
        "tests/compile/ref_nounwindsafe.rs",
        "tests/compile/scope_callback_capture.rs",
        "tests/compile/scope_invariance.rs",
        "tests/compile/scope_mutable_aliasing.rs",
        "tests/compile/scope_userdata_borrow.rs",
        "tests/compile/userdata_internals_hidden.rs",
        "tests/compile/util_helpers_hidden.rs",
        "tests/compile/async_any_userdata_method.rs",
        "tests/compile/async_nonstatic_userdata.rs",
    ];

    const PASS_FIXTURES: &[&str] = &["tests/compile/non_send.rs"];

    #[test]
    fn test_compilation() {
        let t = trybuild::TestCases::new();

        assert_all_fixtures_are_listed();
        for fixture in COMPILE_FAIL_FIXTURES {
            t.compile_fail(fixture);
        }
        for fixture in PASS_FIXTURES {
            t.pass(fixture);
        }
    }

    fn assert_all_fixtures_are_listed() {
        let listed = COMPILE_FAIL_FIXTURES
            .iter()
            .chain(PASS_FIXTURES)
            .copied()
            .collect::<BTreeSet<_>>();

        let compile_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compile");
        let discovered = fs::read_dir(&compile_dir)
            .expect("compile fixture dir")
            .filter_map(|entry| {
                let path = entry.expect("compile fixture").path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    return None;
                }
                let file_name = path
                    .file_name()
                    .expect("fixture file name")
                    .to_str()
                    .expect("compile fixture file names should be UTF-8 for trybuild paths");
                Some(format!("tests/compile/{file_name}"))
            })
            .collect::<BTreeSet<_>>();
        let discovered = discovered
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();

        assert_eq!(discovered, listed);
    }
}
