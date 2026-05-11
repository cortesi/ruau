//! README example.

// snips-start: main
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
// snips-end: main
