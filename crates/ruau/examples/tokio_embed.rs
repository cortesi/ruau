//! Embeds checked Luau modules in a Tokio application.

use std::error::Error;

use ruau::{
    HostApi, Luau, Result as LuauResult,
    analyzer::Checker,
    resolver::{InMemoryResolver, ResolverSnapshot},
};
use tokio::task::{LocalSet, spawn_local, yield_now};

/// Simulates an async host lookup.
async fn fetch(_lua: &Luau, key: String) -> LuauResult<String> {
    yield_now().await;
    Ok(format!("value:{key}"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    LocalSet::new().run_until(run()).await
}

async fn run() -> Result<(), Box<dyn Error>> {
    let host = HostApi::new().try_namespace("host", |ns| {
        ns.try_async_function("fetch", fetch, "(key: string) -> string")?;
        Ok(())
    })?;

    let resolver = InMemoryResolver::new()
        .with_module(
            "main",
            "local dep = require('dep')\nreturn host.fetch(dep.key)",
        )
        .with_module("dep", "return { key = 'project' }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "main").await?;
    let value: String = spawn_local(async move {
        let mut checker = Checker::new()?;
        host.install_definitions(&mut checker)?;

        let lua = Luau::new();
        host.install(&lua)?;

        let value = lua
            .checked_load(&mut checker, snapshot)
            .await?
            .eval()
            .await?;
        Ok::<String, Box<dyn Error>>(value)
    })
    .await??;

    assert_eq!("value:project", value);
    Ok(())
}
