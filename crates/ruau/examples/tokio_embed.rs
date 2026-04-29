//! Embeds checked Luau modules in a Tokio application.

use ruau::{
    HostApi, Luau, Result,
    analyzer::Checker,
    resolver::{InMemoryResolver, ResolverSnapshot},
};
use tokio::task::yield_now;

/// Simulates an async host lookup.
async fn fetch(_lua: &Luau, key: String) -> Result<String> {
    yield_now().await;
    Ok(format!("value:{key}"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let host = HostApi::new().global_async_function(
        "fetch",
        fetch,
        "declare function fetch(key: string): string",
    );

    let mut checker = Checker::new().expect("checker");
    host.add_definitions_to(&mut checker)
        .expect("host definitions");

    let lua = Luau::new();
    host.install(&lua)?;

    let resolver = InMemoryResolver::new()
        .with_module("main", "local dep = require('dep')\nreturn fetch(dep.key)")
        .with_module("dep", "return { key = 'project' }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "main").expect("snapshot");
    let value: String = lua
        .checked_load(&mut checker, snapshot)
        .expect("checked load")
        .eval()
        .await?;

    assert_eq!("value:project", value);
    Ok(())
}
