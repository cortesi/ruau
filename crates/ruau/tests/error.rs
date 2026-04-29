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

use std::{error::Error as _, fmt, io};

use ruau::{Error, ErrorContext, Lua, Result};

#[tokio::test]
async fn test_error_context() -> Result<()> {
    let lua = Lua::new();

    let func = lua.create_function(|_, ()| {
        Err::<(), _>(Error::runtime("runtime error")).context("some context")
    })?;
    lua.globals().set("func", func)?;

    let msg = lua
        .load("local _, err = pcall(func); return tostring(err)")
        .eval::<String>()
        .await?;
    assert!(msg.contains("some context"));
    assert!(msg.contains("runtime error"));

    let func2 = lua.create_function(|lua, ()| {
        lua.globals()
            .get::<String>("nonextant")
            .with_context(|_| "failed to find global")
    })?;
    lua.globals().set("func2", func2)?;

    let msg2 = lua
        .load("local _, err = pcall(func2); return tostring(err)")
        .eval::<String>()
        .await?;
    assert!(msg2.contains("failed to find global"));
    assert!(msg2.contains("error converting Lua nil to String"));

    // Rewrite context message and test `downcast_ref`
    let func3 = lua.create_function(|_, ()| {
        Err::<(), _>(Error::external(io::Error::other("other")))
            .context("some context")
            .context("some new context")
    })?;
    let err = func3.call::<()>(()).await.unwrap_err();
    let err = err.parent().unwrap();
    assert!(!err.to_string().contains("some context"));
    assert!(err.to_string().contains("some new context"));
    assert!(err.downcast_ref::<io::Error>().is_some());
    assert!(err.downcast_ref::<fmt::Error>().is_none());

    Ok(())
}

#[tokio::test]
async fn test_error_chain() -> Result<()> {
    let lua = Lua::new();

    // Check that `Error::ExternalError` creates a chain with a single element
    let io_err = io::Error::other("other");
    assert_eq!(Error::external(io_err).chain().count(), 1);

    let func = lua.create_function(|_, ()| {
        let err = Error::external(io::Error::other("other")).context("io error");
        Err::<(), _>(err)
    })?;
    let err = func.call::<()>(()).await.unwrap_err();
    assert_eq!(err.chain().count(), 3);
    for (i, err) in err.chain().enumerate() {
        match i {
            0 => assert!(matches!(
                err.downcast_ref(),
                Some(Error::CallbackError { .. })
            )),
            1 => assert!(matches!(
                err.downcast_ref(),
                Some(Error::WithContext { .. })
            )),
            2 => assert!(matches!(err.downcast_ref(), Some(io::Error { .. }))),
            _ => unreachable!(),
        }
    }

    let err = err.parent().unwrap();
    assert!(err.source().is_none()); // The source is included to the `Display` output
    assert!(err.to_string().contains("io error"));
    assert!(err.to_string().contains("other"));

    Ok(())
}

#[tokio::test]
async fn test_external_error() {
    // `Error::external` should preserve `ruau::Error`
    let runtime_err = Error::runtime("test error");
    let converted = Error::external(runtime_err);
    assert!(matches!(converted, Error::RuntimeError(ref msg) if msg == "test error"));

    // Other errors should become `ExternalError`
    let converted = Error::external(io::Error::other("other error"));
    assert!(matches!(converted, Error::ExternalError(_)));
    assert!(converted.downcast_ref::<io::Error>().is_some());
}

#[cfg(feature = "anyhow")]
#[tokio::test]
async fn test_error_anyhow() -> Result<()> {
    use ruau::IntoLua;

    let lua = Lua::new();

    let err = anyhow::Error::msg("anyhow error");
    let val = err.into_lua(&lua)?;
    assert!(val.is_error());
    assert_eq!(
        val.as_error().unwrap().to_string(),
        "runtime error: anyhow error"
    );

    let err = anyhow::Error::msg("root cause").context("outer context");
    let val = err.into_lua(&lua)?;
    let msg = val.as_error().unwrap().to_string();
    assert!(msg.contains("outer context"));
    assert!(msg.contains("runtime error: root cause"));

    Ok(())
}
