//! Checked host/session example.
//!
//! Run with `cargo run -p ruau --example checked_session`.

use std::error::Error;

use ruau::{CheckedHost, HostApi, Luau, analyzer::Checker, resolver::InMemoryResolver};

const HOST_DECLARATIONS: &str = r#"
-- Hand-written host API declaration. In a larger embedder this would usually
-- come from a `.d.luau` file checked into the host crate.
declare host: {
    greet: (name: string) -> string,
}
"#;

const LABEL_DECLARATION: &str = r#"
-- Requireable declaration module supplied by the host catalog.
export type Module = {
    tag: (value: string) -> string,
}
"#;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let host_api = HostApi::new()
        .add_definition_for("host", HOST_DECLARATIONS)
        .add_installer("host", |lua| {
            let host = lua.create_table()?;
            host.set(
                "greet",
                lua.create_function(|_, name: String| Ok(format!("hello, {name}")))?,
            )?;
            lua.globals().set("host", host)
        });

    let host = CheckedHost::from_host_api(host_api).with_interface("labels", LABEL_DECLARATION)?;

    let mut checker = Checker::new()?;
    let interface_check = host
        .check_script(
            &mut checker,
            r#"
local labels = require("labels")
local tagged: string = labels.tag("demo")
"#,
        )
        .await?;
    assert!(interface_check.is_ok(), "{interface_check:#?}");

    let resolver = InMemoryResolver::new()
        .with_module(
            "main",
            r#"
local util = require("util")
return host.greet(util.name())
"#,
        )
        .with_module(
            "util",
            r#"
return {
    name = function()
        return "ruau"
    end,
}
"#,
        );

    let lua = Luau::new();
    host.install_runtime(&lua).await?;

    let greeting: String = host
        .checked_load_resolved(&lua, &mut checker, &resolver, "main")
        .await?
        .eval()
        .await?;
    assert_eq!(greeting, "hello, ruau");

    Ok(())
}
