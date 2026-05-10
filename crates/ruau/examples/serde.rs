//! Converts Luau values to and from Serde-compatible Rust types.

use ruau::{Error, Luau, Result, Value};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
enum Transmission {
    Manual,
    Automatic,
}

#[derive(Serialize, Deserialize)]
struct Engine {
    v: u32,
    kw: u32,
}

#[derive(Serialize, Deserialize)]
struct Car {
    active: bool,
    model: String,
    transmission: Transmission,
    engine: Engine,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let lua = Luau::new();
    let globals = lua.globals();

    // Create Car struct from a Luau table
    let car: Car = lua.deserialize_value(
        lua.load(
            r#"
        {active = true, model = "Volkswagen Golf", transmission = "Automatic", engine = {v = 1499, kw = 90}}
    "#,
        )
        .eval()
        .await?,
    )?;

    // Set it as ordinary Luau data
    globals.set("null", Value::NULL)?;
    globals.set("array_mt", lua.array_metatable())?;
    globals.set("car", lua.to_value(&car)?)?;

    // Create a Luau table with multiple data types
    let val: Value = lua
        .load(r#"{driver = "Boris", car = car, price = null, points = setmetatable({}, array_mt)}"#)
        .eval()
        .await?;

    // Serialize the table above to JSON
    let json_str = serde_json::to_string(&val).map_err(Error::external)?;
    println!("{}", json_str);

    // Create Luau Value from JSON (or any serializable type)
    let json = serde_json::json!({
        "key": "value",
        "null": null,
        "array": [],
    });
    globals.set("json_value", lua.to_value(&json)?)?;
    lua.load(
        r#"
        assert(json_value["key"] == "value")
        assert(json_value["null"] == null)
        assert(#(json_value["array"]) == 0)
    "#,
    )
    .exec()
    .await?;

    Ok(())
}
