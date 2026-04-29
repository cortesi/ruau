# ruau

[![Latest Version]][crates.io] [![API Documentation]][docs.rs] ![MSRV]

[Latest Version]: https://img.shields.io/crates/v/ruau.svg
[crates.io]: https://crates.io/crates/ruau
[API Documentation]: https://docs.rs/ruau/badge.svg
[docs.rs]: https://docs.rs/ruau
[MSRV]: https://img.shields.io/badge/rust-1.88+-brightgreen.svg?&logo=rust

[Guided Tour] | [Benchmarks]

[Guided Tour]: examples/guided_tour.rs
[Benchmarks]: https://github.com/khvzak/script-bench-rs

`ruau` is a Rust toolkit for embedding [Luau]: it pairs a safe VM API with checked loading,
resolver snapshots, host API declarations, async execution, and serde integration.

[Luau]: https://luau.org

## Usage

The runtime is Luau, built from the vendored Luau source package.

Available feature flags:

* `macros`: enable procedural macros such as `chunk!`.

### Checked Loading

Use `ruau::analyzer::Checker` with a resolver to analyze Luau sources before execution. A
`resolver::ResolverSnapshot` captures the resolved module graph once so `checked_load_resolved`
uses the same source set for analysis and runtime `require`.

Host APIs can be described with `HostApi`, which keeps the Rust installer and matching `.d.luau`
declaration together. Add the declaration to the checker, then install the host functions into the
VM before executing checked code.

### Async/await Support

Async support is always available and uses Luau coroutines. `Luau` is a single-owner VM handle, so
applications that spawn local Luau work should use a current-thread Tokio runtime with
`tokio::task::LocalSet`.

```shell
cargo run --example async_http_client --features=macros
cargo run --example async_http_reqwest --features=macros
```

### Serde Support

`ruau` can serialize and deserialize values that implement [`serde::Serialize`] and [`serde::Deserialize`] into and from [`ruau::Value`].

[`serde::Serialize`]: https://docs.serde.rs/serde/ser/trait.Serialize.html
[`serde::Deserialize`]: https://docs.serde.rs/serde/de/trait.Deserialize.html
[`ruau::Value`]: https://docs.rs/ruau/latest/ruau/enum.Value.html

### Standalone Mode

```toml
[dependencies]
ruau = { version = "0.12", features = ["macros"] }
```

```rust
use ruau::{Luau, Result};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let lua = Luau::new();

    let map_table = lua.create_table()?;
    map_table.set(1, "one")?;
    map_table.set("two", 2)?;

    lua.globals().set("map_table", map_table)?;
    lua.load("for k,v in pairs(map_table) do print(k,v) end")
        .exec()
        .await?;

    Ok(())
}
```

## Safety

`ruau` aims to provide a safe API between Rust and Luau. Operations that may trigger a Luau error are protected, and users do not interact directly with the raw Luau stack in safe APIs.
