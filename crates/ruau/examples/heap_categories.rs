//! Tracks Luau heap usage by memory category.

use ruau::{Luau, Result};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let lua = Luau::new();

    lua.set_memory_category("startup")?;
    lua.globals().set("startup_name", "ruau")?;

    lua.set_memory_category("scripts")?;
    lua.load(
        r#"
        local records = {}
        for index = 1, 32 do
            records[index] = { id = index, label = "record:" .. index }
        end
        _G.records = records
        "#,
    )
    .exec()
    .await?;

    lua.gc_collect()?;

    let dump = lua.heap_dump()?;
    let categories = dump.size_by_category();
    let scripts_size = categories.get("scripts").copied().unwrap_or_default();
    let table_count = dump
        .size_by_type(Some("scripts"))
        .get("table")
        .map(|(count, _)| *count)
        .unwrap_or_default();

    println!(
        "heap={} scripts={} tables={}",
        dump.size(),
        scripts_size,
        table_count
    );

    assert!(scripts_size > 0);
    assert!(table_count > 0);
    Ok(())
}
