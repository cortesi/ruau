# ruau

[![Latest Version]][crates.io] [![API Documentation]][docs.rs] ![MSRV]

[Latest Version]: https://img.shields.io/crates/v/ruau.svg
[crates.io]: https://crates.io/crates/ruau
[API Documentation]: https://docs.rs/ruau/badge.svg
[docs.rs]: https://docs.rs/ruau
[MSRV]: https://img.shields.io/badge/rust-1.88+-brightgreen.svg?&logo=rust

[Guided Tour] | [Benchmarks]

[Guided Tour]: crates/ruau/examples/guided_tour.rs
[Benchmarks]: https://github.com/khvzak/script-bench-rs

`ruau` provides safe, high-level Rust bindings to [Luau].

[Luau]: https://luau.org

## Usage

Luau support is enabled by default and built from the vendored Luau source package. Other Lua runtimes and LuaJIT are not supported by this project.

Available feature flags:

* `serde`: add serialization and deserialization support using [serde].
* `macros`: enable procedural macros such as `chunk!`.
* `anyhow`: enable `anyhow::Error` conversion into Luau errors.

[serde]: https://github.com/serde-rs/serde

### Async/await Support

Async support is always available and uses Luau coroutines. `Luau` is a single-owner VM handle, so
applications that spawn local Luau work should use a current-thread Tokio runtime with
`tokio::task::LocalSet`.

```shell
cargo run --example async_http_client --features=macros
cargo run --example async_http_reqwest --features=macros,serde
```

### Serde Support

With the `serde` feature flag enabled, `ruau` can serialize and deserialize values that implement [`serde::Serialize`] and [`serde::Deserialize`] into and from [`ruau::Value`].

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
