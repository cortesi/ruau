# ruau

[![Latest Version]][crates.io] [![API Documentation]][docs.rs] ![MSRV]

[Latest Version]: https://img.shields.io/crates/v/ruau.svg
[crates.io]: https://crates.io/crates/ruau
[API Documentation]: https://docs.rs/ruau/badge.svg
[docs.rs]: https://docs.rs/ruau
[MSRV]: https://img.shields.io/badge/rust-1.88%2B-blue.svg

`ruau` embeds [Luau] in Rust with a safe VM API, async execution, serde
conversion, checked loading, and typed host APIs.

[Luau]: https://luau.org

## Checked Example

This example checks a Luau implementation against a `.d.luau`-style module
interface before loading it from the same resolver graph.

<!-- snips: crates/ruau/examples/readme.rs#main -->
```rust
use std::error::Error;

use ruau::{
    CheckedHost, Luau,
    analyzer::Checker,
    resolver::{InMemoryResolver, ModuleId},
};

const GREETER_INTERFACE: &str = r#"
export type Module = {
    greet: (name: string) -> string,
}
"#;

const GREETER_IMPL: &str = r#"
return {
    greet = function(name: string): string
        return "hello, " .. name
    end,
}
"#;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let host = CheckedHost::new().with_interface("greeter", GREETER_INTERFACE)?;
    let mut checker = Checker::new()?;

    let contract = host
        .check_implementation(
            &mut checker,
            GREETER_IMPL,
            &ModuleId::new("greeter_impl"),
            "greeter",
        )
        .await?;
    assert!(contract.is_ok(), "{contract:#?}");

    let resolver = InMemoryResolver::new()
        .with_module("main", r#"return require("greeter_impl").greet("ruau")"#)
        .with_module("greeter_impl", GREETER_IMPL);

    let lua = Luau::new();
    let message: String = host
        .checked_load_resolved(&lua, &mut checker, &resolver, "main")
        .await?
        .eval()
        .await?;
    assert_eq!(message, "hello, ruau");

    Ok(())
}
```

## Checked Loading

Use `ruau::analyzer::Checker` with `ruau::resolver` to analyze Luau before
execution. `ResolverSnapshot` fixes the module graph for both analysis and
runtime `require`.

`CheckedHost` keeps declaration interfaces, implementation checks, and checked
loading behind one surface. Static `require` calls are resolved before
execution; dynamic `require` calls are rejected by checked loading.

## Runtime Model

`Luau` and its handles are local: they are `!Send + !Sync`. Use a
current-thread Tokio runtime for direct VM work. Use `LuauWorker` when
multi-thread Tokio tasks need to share one VM through a cloneable handle.

## Features

* `macros`: enables procedural macros such as `chunk!`.

## More Examples

* [guided_tour.rs] is the longer API tour.
* [tokio_worker.rs] shows multi-thread Tokio integration.
* [serde.rs] shows value conversion with `serde`.

[guided_tour.rs]: crates/ruau/examples/guided_tour.rs
[tokio_worker.rs]: crates/ruau/examples/tokio_worker.rs
[serde.rs]: crates/ruau/examples/serde.rs

## Safety

`ruau` protects operations that may raise Luau errors and keeps the raw Luau
stack behind safe Rust APIs.
