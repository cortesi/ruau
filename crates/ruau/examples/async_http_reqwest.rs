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

use ruau::{ExternalResult, Luau, Result, Value, chunk};
use tokio::task::LocalSet;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    LocalSet::new().run_until(run()).await
}

async fn run() -> Result<()> {
    let lua = Luau::new();
    let client = reqwest::Client::new();

    let fetch_json = lua.create_async_function(async move |lua, uri: String| {
        let resp = client
            .get(&uri)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .into_luau_err()?;
        let json = resp.json::<serde_json::Value>().await.into_luau_err()?;
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

    f.call("https://httpbin.org/anything?arg0=val0").await
}
