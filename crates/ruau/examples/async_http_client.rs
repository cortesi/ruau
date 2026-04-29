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

use std::collections::HashMap;

use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use hyper_util::{client::legacy::Client as HyperClient, rt::TokioExecutor};
use ruau::{ExternalResult, Luau, Result, UserData, UserDataMethods, chunk};
use tokio::task::LocalSet;

struct BodyReader(Incoming);

impl UserData for BodyReader {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Every call returns a next chunk
        methods.add_async_method_mut("read", async |lua, mut reader, ()| {
            if let Some(bytes) = reader.0.frame().await
                && let Some(bytes) = bytes.into_luau_err()?.data_ref()
            {
                return Some(lua.create_string(bytes)).transpose();
            }
            Ok(None)
        });
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    LocalSet::new().run_until(run()).await
}

async fn run() -> Result<()> {
    let lua = Luau::new();
    let client = HyperClient::builder(TokioExecutor::new()).build_http::<String>();

    let fetch_url = lua.create_async_function(async move |lua, uri: String| {
        let client = client.clone();
        let uri = uri.parse().into_luau_err()?;
        let resp = client.get(uri).await.into_luau_err()?;

        let lua_resp = lua.create_table()?;
        lua_resp.set("status", resp.status().as_u16())?;

        let mut headers = HashMap::new();
        for (key, value) in resp.headers() {
            headers
                .entry(key.as_str())
                .or_insert(Vec::new())
                .push(value.to_str().into_luau_err()?);
        }

        lua_resp.set("headers", headers)?;
        lua_resp.set("body", BodyReader(resp.into_body()))?;

        Ok(lua_resp)
    })?;

    let f = lua
        .load(chunk! {
            local res = $fetch_url(...)
            print("status: "..res.status)
            for key, vals in pairs(res.headers) do
                for _, val in ipairs(vals) do
                    print(key..": "..val)
                end
            end
            repeat
                local chunk = res.body:read()
                if chunk then
                    print(chunk)
                end
            until not chunk
        })
        .into_function()?;

    f.call("http://httpbin.org/ip").await
}
