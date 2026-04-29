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

use std::panic::catch_unwind;

use ruau::{Error, Function, IntoLuau, Luau, Result, Thread, Value};

#[tokio::test]
async fn test_thread() -> Result<()> {
    let lua = Luau::new();

    let thread = lua.create_thread(
        lua.load(
            r#"
            function (s)
                local sum = s
                for i = 1,4 do
                    sum = sum + coroutine.yield(sum)
                end
                return sum
            end
            "#,
        )
        .eval()
        .await?,
    )?;

    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(0)?, 0);
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(1)?, 1);
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(2)?, 3);
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(3)?, 6);
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(4)?, 10);
    assert!(thread.is_finished());

    let accumulate = lua.create_thread(
        lua.load(
            r#"
            function (sum)
                while true do
                    sum = sum + coroutine.yield(sum)
                end
            end
            "#,
        )
        .eval::<Function>()
        .await?,
    )?;

    for i in 0..4 {
        accumulate.resume::<()>(i)?;
    }
    assert_eq!(accumulate.resume::<i64>(4)?, 10);
    assert!(accumulate.is_resumable());
    assert!(accumulate.resume::<()>("error").is_err());
    assert!(accumulate.is_error());

    let thread = lua
        .load(
            r#"
            coroutine.create(function ()
                while true do
                    coroutine.yield(42)
                end
            end)
        "#,
        )
        .eval::<Thread>()
        .await?;
    assert!(thread.is_resumable());
    assert_eq!(thread.resume::<i64>(())?, 42);

    let thread: Thread = lua
        .load(
            r#"
            coroutine.create(function(arg)
                assert(arg == 42)
                local yieldarg = coroutine.yield(123)
                assert(yieldarg == 43)
                return 987
            end)
        "#,
        )
        .eval()
        .await?;

    assert_eq!(thread.resume::<u32>(42)?, 123);
    assert_eq!(thread.resume::<u32>(43)?, 987);

    match thread.resume::<u32>(()) {
        Err(Error::CoroutineUnresumable) => {}
        Err(_) => panic!("resuming dead coroutine error is not CoroutineInactive kind"),
        _ => panic!("resuming dead coroutine did not return error"),
    }

    // Already running thread must be unresumable
    let thread = lua.create_thread(lua.create_function(|lua, ()| {
        assert!(lua.current_thread().is_running());
        let result = lua.current_thread().resume::<()>(());
        assert!(
            matches!(result, Err(Error::CoroutineUnresumable)),
            "unexpected result: {result:?}",
        );
        Ok(())
    })?)?;
    let result = thread.resume::<()>(());
    assert!(result.is_ok(), "unexpected result: {result:?}");

    Ok(())
}

#[tokio::test]
async fn test_thread_reset() -> Result<()> {
    use std::sync::Arc;

    use ruau::{AnyUserData, UserData};

    let lua = Luau::new();

    struct MyUserData(#[allow(unused)] Arc<()>);
    impl UserData for MyUserData {}

    let arc = Arc::new(());

    let func: Function = lua.load(r#"function(ud) coroutine.yield(ud) end"#).eval().await?;
    let thread = lua.create_thread(lua.load("return 0").into_function()?)?; // Dummy function first
    assert!(thread.reset(func.clone()).is_ok());

    for _ in 0..2 {
        assert!(thread.is_resumable());
        let _ = thread.resume::<AnyUserData>(MyUserData(arc.clone()))?;
        assert!(thread.is_resumable());
        assert_eq!(Arc::strong_count(&arc), 2);
        thread.resume::<()>(())?;
        assert!(thread.is_finished());
        thread.reset(func.clone())?;
        lua.gc_collect()?;
        assert_eq!(Arc::strong_count(&arc), 1);
    }

    // Check for errors
    let func: Function = lua.load(r#"function(ud) error("test error") end"#).eval().await?;
    let thread = lua.create_thread(func.clone())?;
    let _ = thread.resume::<AnyUserData>(MyUserData(arc.clone()));
    assert!(thread.is_error());
    assert_eq!(Arc::strong_count(&arc), 2);
    assert!(thread.reset(func).is_ok());
    assert!(thread.is_resumable());

    // Try reset running thread
    let thread = lua.create_thread(lua.create_function(|lua, ()| {
        let this = lua.current_thread();
        this.reset(lua.create_function(|_, ()| Ok(()))?)?;
        Ok(())
    })?)?;
    let result = thread.resume::<()>(());
    assert!(
        matches!(result, Err(Error::CallbackError{ ref cause, ..})
            if matches!(cause.as_ref(), Error::RuntimeError(err)
                if err == "cannot reset a running thread")
        ),
        "unexpected result: {result:?}",
    );

    Ok(())
}

#[tokio::test]
async fn test_coroutine_from_closure() -> Result<()> {
    let lua = Luau::new();

    let thrd_main = lua.create_function(|_, ()| Ok(()))?;
    lua.globals().set("main", thrd_main)?;

    let thrd: Thread = lua.load("coroutine.create(main)").eval().await?;

    thrd.resume::<()>(())?;

    Ok(())
}

#[tokio::test]
#[cfg(not(panic = "abort"))]
async fn test_coroutine_panic() {
    match catch_unwind(|| -> Result<()> {
        // check that coroutines propagate panics correctly
        let lua = Luau::new();
        let thrd_main = lua.create_function(|_, ()| -> Result<()> {
            panic!("test_panic");
        })?;
        lua.globals().set("main", &thrd_main)?;
        let thrd: Thread = lua.create_thread(thrd_main)?;
        thrd.resume(())
    }) {
        Ok(r) => panic!("coroutine panic not propagated, instead returned {:?}", r),
        Err(p) => assert!(*p.downcast::<&str>().unwrap() == "test_panic"),
    }
}

#[tokio::test]
async fn test_thread_pointer() -> Result<()> {
    let lua = Luau::new();

    let func = lua.load("return 123").into_function()?;
    let thread = lua.create_thread(func)?;

    assert_eq!(thread.to_pointer(), thread.to_pointer());
    assert_ne!(thread.to_pointer(), lua.current_thread().to_pointer());

    Ok(())
}

#[tokio::test]
async fn test_thread_resume_error() -> Result<()> {
    let lua = Luau::new();

    let thread = lua
        .load(
            r#"
        coroutine.create(function()
            local ok, err = pcall(coroutine.yield, 123)
            assert(not ok, "yield should fail")
            assert(err == "myerror", "unexpected error: " .. tostring(err))
            return "success"
        end)
    "#,
        )
        .eval::<Thread>()
        .await?;

    assert_eq!(thread.resume::<i64>(())?, 123);
    let status = thread.resume_error::<String>("myerror").unwrap();
    assert_eq!(status, "success");

    Ok(())
}

#[tokio::test]
async fn test_thread_resume_bad_arg() -> Result<()> {
    let lua = Luau::new();

    struct BadArg;

    impl IntoLuau for BadArg {
        fn into_luau(self, _lua: &Luau) -> Result<Value> {
            Err(Error::runtime("bad arg"))
        }
    }

    let f = lua.create_thread(lua.create_function(|_, ()| Ok("okay"))?)?;
    let res = f.resume::<()>((123, BadArg));
    assert!(matches!(res, Err(Error::RuntimeError(msg)) if msg == "bad arg"));
    let res = f.resume::<String>(()).unwrap();
    assert_eq!(res, "okay");

    Ok(())
}
