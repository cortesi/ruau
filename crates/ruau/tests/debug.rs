//! debug integration tests.

use ruau::{Luau, Result};

#[cfg(test)]
mod tests {
    use super::*;

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
}
