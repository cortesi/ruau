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

use ruau::{ExternalResult, Lua, LuaSerdeExt, Result, Value, chunk};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let lua = Lua::new();

    let fetch_json = lua.create_async_function(async |lua, uri: String| {
        let resp = reqwest::get(&uri)
            .await
            .and_then(|resp| resp.error_for_status())
            .into_lua_err()?;
        let json = resp.json::<serde_json::Value>().await.into_lua_err()?;
        lua.to_value(&json)
    })?;

    let dbg = lua.create_function(|_, value: Value| {
        println!("{value:#?}");
        Ok(())
    })?;

    let f = lua
        .load(chunk! {
            local res = $fetch_json(...)
            $dbg(res)
        })
        .into_function()?;

    f.call_async("https://httpbin.org/anything?arg0=val0").await
}
