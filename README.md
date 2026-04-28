# mlua

[![Build Status]][github-actions] [![Latest Version]][crates.io] [![API Documentation]][docs.rs] [![Coverage Status]][codecov.io] ![MSRV]

[Build Status]: https://github.com/mlua-rs/mlua/workflows/CI/badge.svg
[github-actions]: https://github.com/mlua-rs/mlua/actions
[Latest Version]: https://img.shields.io/crates/v/mlua.svg
[crates.io]: https://crates.io/crates/mlua
[API Documentation]: https://docs.rs/mlua/badge.svg
[docs.rs]: https://docs.rs/mlua
[Coverage Status]: https://codecov.io/gh/mlua-rs/mlua/branch/main/graph/badge.svg?token=99339FS1CG
[codecov.io]: https://codecov.io/gh/mlua-rs/mlua
[MSRV]: https://img.shields.io/badge/rust-1.88+-brightgreen.svg?&logo=rust

[Guided Tour] | [Benchmarks]

[Guided Tour]: examples/guided_tour.rs
[Benchmarks]: https://github.com/khvzak/script-bench-rs

`mlua` provides safe, high-level Rust bindings to [Luau].

[Luau]: https://luau.org

## Usage

Luau support is enabled by default and built from the vendored Luau source package. Other Lua runtimes and LuaJIT are not supported by this project.

Available feature flags:

* `luau`: enable Luau support. This is included in the default feature set.
* `luau-jit`: enable the Luau JIT backend.
* `luau-vector4`: enable 4-dimensional Luau vectors.
* `async`: enable async/await support.
* `send`: make `mlua::Lua: Send + Sync`.
* `error-send`: make `mlua::Error: Send + Sync`.
* `serde`: add serialization and deserialization support using [serde].
* `macros`: enable procedural macros such as `chunk!`.
* `anyhow`: enable `anyhow::Error` conversion into Lua errors.
* `userdata-wrappers`: implement `UserData` for common wrapper types when `T: UserData`.

[serde]: https://github.com/serde-rs/serde

### Async/await Support

Async support uses Luau coroutines and requires enabling `feature = "async"` in `Cargo.toml`.

```shell
cargo run --example async_http_client --features=async,macros
cargo run --example async_http_reqwest --features=async,macros,serde
cargo run --example async_http_server --features=async,macros,send
```

### Serde Support

With the `serde` feature flag enabled, `mlua` can serialize and deserialize values that implement [`serde::Serialize`] and [`serde::Deserialize`] into and from [`mlua::Value`].

[`serde::Serialize`]: https://docs.serde.rs/serde/ser/trait.Serialize.html
[`serde::Deserialize`]: https://docs.serde.rs/serde/de/trait.Deserialize.html
[`mlua::Value`]: https://docs.rs/mlua/latest/mlua/enum.Value.html

### Standalone Mode

```toml
[dependencies]
mlua = { version = "0.12", features = ["macros"] }
```

```rust
use mlua::prelude::*;

fn main() -> LuaResult<()> {
    let lua = Lua::new();

    let map_table = lua.create_table()?;
    map_table.set(1, "one")?;
    map_table.set("two", 2)?;

    lua.globals().set("map_table", map_table)?;
    lua.load("for k,v in pairs(map_table) do print(k,v) end").exec()?;

    Ok(())
}
```

## Safety

`mlua` aims to provide a safe API between Rust and Luau. Operations that may trigger a Luau error are protected, and users do not interact directly with the raw Luau stack in safe APIs.
