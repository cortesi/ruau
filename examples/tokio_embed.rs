//! Embeds checked Luau modules in a Tokio application.
#![allow(
    clippy::absolute_paths,
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

use ruau::{
    HostApi, Luau, Result,
    analyzer::Checker,
    resolver::{InMemoryResolver, ResolverSnapshot},
};
use tokio::task::{LocalSet, yield_now};

/// Simulates an async host lookup.
async fn fetch(_lua: &Luau, key: String) -> Result<String> {
    yield_now().await;
    Ok(format!("value:{key}"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    LocalSet::new().run_until(run()).await
}

async fn run() -> Result<()> {
    let host = HostApi::new().namespace("host", |ns| {
        ns.async_function("fetch", fetch, "(key: string) -> string");
    });

    let resolver = InMemoryResolver::new()
        .with_module("main", "local dep = require('dep')\nreturn host.fetch(dep.key)")
        .with_module("dep", "return { key = 'project' }");
    let snapshot = ResolverSnapshot::resolve(&resolver, "main")
        .await
        .expect("snapshot");
    let value: String = tokio::task::spawn_local(async move {
        let mut checker = Checker::new().expect("checker");
        host.add_definitions_to(&mut checker).expect("host definitions");

        let lua = Luau::new();
        host.install(&lua)?;

        lua.checked_load(&mut checker, snapshot)
            .await
            .expect("checked load")
            .eval()
            .await
    })
    .await
    .expect("local task")?;

    assert_eq!("value:project", value);
    Ok(())
}
