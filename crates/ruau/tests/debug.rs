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

use ruau::{Luau, Result};

#[tokio::test]
async fn test_debug_format() -> Result<()> {
    let lua = Luau::new();

    // Globals
    let globals = lua.globals();
    let dump = format!("{globals:#?}");
    assert!(dump.starts_with("{\n  _G = table:"));

    // TODO: Other cases

    Ok(())
}
