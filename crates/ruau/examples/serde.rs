#![allow(
    missing_docs,
    clippy::absolute_paths,
    clippy::missing_docs_in_private_items,
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

use ruau::{Error, Lua, LuaSerdeExt, Result, UserData, Value};
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

impl UserData for Car {}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let lua = Lua::new();
    let globals = lua.globals();

    // Create Car struct from a Lua table
    let car: Car = lua.from_value(
        lua.load(
            r#"
        {active = true, model = "Volkswagen Golf", transmission = "Automatic", engine = {v = 1499, kw = 90}}
    "#,
        )
        .eval()
        .await?,
    )?;

    // Set it as (serializable) userdata
    globals.set("null", lua.null())?;
    globals.set("array_mt", lua.array_metatable())?;
    globals.set("car", lua.create_ser_userdata(car)?)?;

    // Create a Lua table with multiple data types
    let val: Value = lua
        .load(r#"{driver = "Boris", car = car, price = null, points = setmetatable({}, array_mt)}"#)
        .eval()
        .await?;

    // Serialize the table above to JSON
    let json_str = serde_json::to_string(&val).map_err(Error::external)?;
    println!("{}", json_str);

    // Create Lua Value from JSON (or any serializable type)
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
