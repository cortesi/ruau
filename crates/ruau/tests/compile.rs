#![allow(
    missing_docs,
    clippy::absolute_paths,
    clippy::missing_docs_in_private_items,
    clippy::tests_outside_test_module,
    clippy::items_after_statements,
    clippy::cognitive_complexity,
    clippy::let_underscore_must_use,
    clippy::manual_c_str_literals,
    clippy::mutable_key_type,
    clippy::needless_maybe_sized,
    clippy::needless_pass_by_value,
    clippy::redundant_pattern_matching
)]

#[test]
#[ignore = "trybuild stderr output is compiler-version-sensitive"]
fn test_compilation() {
    let t = trybuild::TestCases::new();

    t.compile_fail("tests/compile/function_borrow.rs");
    t.compile_fail("tests/compile/lua_norefunwindsafe.rs");
    t.compile_fail("tests/compile/ref_nounwindsafe.rs");
    t.compile_fail("tests/compile/scope_callback_capture.rs");
    t.compile_fail("tests/compile/scope_invariance.rs");
    t.compile_fail("tests/compile/scope_mutable_aliasing.rs");
    t.compile_fail("tests/compile/scope_userdata_borrow.rs");

    #[cfg(feature = "async")]
    {
        t.compile_fail("tests/compile/async_any_userdata_method.rs");
        t.compile_fail("tests/compile/async_nonstatic_userdata.rs");
    }

    #[cfg(feature = "send")]
    t.compile_fail("tests/compile/non_send.rs");
    #[cfg(not(feature = "send"))]
    t.pass("tests/compile/non_send.rs");
}
