//! compile integration tests.

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "trybuild stderr output is compiler-version-sensitive"]
    fn test_compilation() {
        let t = trybuild::TestCases::new();

        t.compile_fail("tests/compile/function_borrow.rs");
        t.compile_fail("tests/compile/internal_state_private.rs");
        t.compile_fail("tests/compile/luau_norefunwindsafe.rs");
        t.compile_fail("tests/compile/raw_internals_hidden.rs");
        t.compile_fail("tests/compile/runtime_require_private.rs");
        t.compile_fail("tests/compile/ref_nounwindsafe.rs");
        t.compile_fail("tests/compile/scope_callback_capture.rs");
        t.compile_fail("tests/compile/scope_invariance.rs");
        t.compile_fail("tests/compile/scope_mutable_aliasing.rs");
        t.compile_fail("tests/compile/scope_userdata_borrow.rs");
        t.compile_fail("tests/compile/userdata_internals_hidden.rs");
        t.compile_fail("tests/compile/util_helpers_hidden.rs");
        {
            t.compile_fail("tests/compile/async_any_userdata_method.rs");
            t.compile_fail("tests/compile/async_nonstatic_userdata.rs");
        }

        t.pass("tests/compile/non_send.rs");
    }
}
