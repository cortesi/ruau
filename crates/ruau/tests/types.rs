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

use std::os::raw::c_void;

use ruau::{Function, LightUserData, Luau, PrimitiveType, Result, Thread};

#[tokio::test]
async fn test_lightuserdata() -> Result<()> {
    let lua = Luau::new();

    let globals = lua.globals();
    lua.load(
        r#"
        function id(a)
            return a
        end
    "#,
    )
    .exec()
    .await?;

    let res = globals
        .get::<Function>("id")?
        .call::<LightUserData>(LightUserData(42 as *mut c_void))
        .await?;

    assert_eq!(res, LightUserData(42 as *mut c_void));

    Ok(())
}

#[tokio::test]
async fn test_boolean_type_metatable() -> Result<()> {
    let lua = Luau::new();

    let mt = lua.create_table()?;
    mt.set(
        "__add",
        lua.create_function(|_, (a, b): (bool, bool)| Ok(a || b))?,
    )?;
    assert_eq!(lua.type_metatable(PrimitiveType::Boolean), None);
    lua.set_type_metatable(PrimitiveType::Boolean, Some(mt.clone()));
    assert_eq!(lua.type_metatable(PrimitiveType::Boolean).unwrap(), mt);

    lua.load(r#"assert(true + true == true)"#)
        .exec()
        .await
        .unwrap();
    lua.load(r#"assert(true + false == true)"#)
        .exec()
        .await
        .unwrap();
    lua.load(r#"assert(false + true == true)"#)
        .exec()
        .await
        .unwrap();
    lua.load(r#"assert(false + false == false)"#)
        .exec()
        .await
        .unwrap();

    Ok(())
}

#[tokio::test]
async fn test_lightuserdata_type_metatable() -> Result<()> {
    let lua = Luau::new();

    let mt = lua.create_table()?;
    mt.set(
        "__add",
        lua.create_function(|_, (a, b): (LightUserData, LightUserData)| {
            Ok(LightUserData((a.0 as usize + b.0 as usize) as *mut c_void))
        })?,
    )?;
    lua.set_type_metatable(PrimitiveType::LightUserData, Some(mt.clone()));
    assert_eq!(
        lua.type_metatable(PrimitiveType::LightUserData).unwrap(),
        mt
    );

    let res = lua
        .load(
            r#"
        local a, b = ...
        return a + b
    "#,
        )
        .call::<LightUserData>((
            LightUserData(42 as *mut c_void),
            LightUserData(100 as *mut c_void),
        ))
        .await
        .unwrap();
    assert_eq!(res, LightUserData(142 as *mut c_void));

    Ok(())
}

#[tokio::test]
async fn test_number_type_metatable() -> Result<()> {
    let lua = Luau::new();

    let mt = lua.create_table()?;
    mt.set(
        "__call",
        lua.create_function(|_, (n1, n2): (f64, f64)| Ok(n1 * n2))?,
    )?;
    lua.set_type_metatable(PrimitiveType::Number, Some(mt.clone()));
    assert_eq!(lua.type_metatable(PrimitiveType::Number).unwrap(), mt);

    lua.load(r#"assert((1.5)(3.0) == 4.5)"#)
        .exec()
        .await
        .unwrap();
    lua.load(r#"assert((5)(5) == 25)"#).exec().await.unwrap();

    Ok(())
}

#[tokio::test]
async fn test_string_type_metatable() -> Result<()> {
    let lua = Luau::new();

    let mt = lua.create_table()?;
    mt.set(
        "__add",
        lua.create_function(|_, (a, b): (String, String)| Ok(format!("{a}{b}")))?,
    )?;
    lua.set_type_metatable(PrimitiveType::String, Some(mt.clone()));
    assert_eq!(lua.type_metatable(PrimitiveType::String).unwrap(), mt);

    lua.load(r#"assert(("foo" + "bar") == "foobar")"#)
        .exec()
        .await
        .unwrap();

    Ok(())
}

#[tokio::test]
async fn test_function_type_metatable() -> Result<()> {
    let lua = Luau::new();

    let mt = lua.create_table()?;
    mt.set(
        "__index",
        lua.create_function(|_, (_, key): (Function, String)| Ok(format!("function.{key}")))?,
    )?;
    lua.set_type_metatable(PrimitiveType::Function, Some(mt.clone()));
    assert_eq!(lua.type_metatable(PrimitiveType::Function), Some(mt));

    lua.load(r#"assert((function() end).foo == "function.foo")"#)
        .exec()
        .await
        .unwrap();

    Ok(())
}

#[tokio::test]
async fn test_thread_type_metatable() -> Result<()> {
    let lua = Luau::new();

    let mt = lua.create_table()?;
    mt.set(
        "__index",
        lua.create_function(|_, (_, key): (Thread, String)| Ok(format!("thread.{key}")))?,
    )?;
    lua.set_type_metatable(PrimitiveType::Thread, Some(mt.clone()));
    assert_eq!(lua.type_metatable(PrimitiveType::Thread), Some(mt));

    lua.load(r#"assert((coroutine.create(function() end)).foo == "thread.foo")"#)
        .exec()
        .await
        .unwrap();

    Ok(())
}
