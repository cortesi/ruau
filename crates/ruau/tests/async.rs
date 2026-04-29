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
use std::{sync::Arc, time::Duration};

use futures_util::stream::TryStreamExt;
use ruau::{
    Error, Function, Luau, LuauOptions, MultiValue, ObjectLike, Result, StdLib, Table, UserData,
    UserDataMethods, UserDataRef, Value,
};
use tokio::sync::Mutex;

#[cfg(not(target_arch = "wasm32"))]
async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(target_arch = "wasm32")]
async fn sleep_ms(_ms: u64) {
    // I was unable to make sleep() work in wasm32-emscripten target
    tokio::task::yield_now().await;
}

#[tokio::test]
async fn test_async_function() -> Result<()> {
    let lua = Luau::new();

    let f = lua.create_async_function(async |_lua, (a, b, c): (i64, i64, i64)| Ok((a + b) * c))?;
    lua.globals().set("f", f)?;

    let res: i64 = lua.load("f(1, 2, 3)").eval().await?;
    assert_eq!(res, 9);

    Ok(())
}

#[tokio::test]
async fn test_async_function_wrap() -> Result<()> {
    let lua = Luau::new();

    let f = Function::wrap_async(|s: String| async {
        tokio::task::yield_now().await;
        Ok::<_, Error>(s)
    });
    lua.globals().set("f", f)?;
    let res: String = lua.load(r#"f("hello")"#).eval().await?;
    assert_eq!(res, "hello");

    // Return error
    let ferr = Function::wrap_async(|| async { Err::<(), _>(Error::runtime("some async error")) });
    lua.globals().set("ferr", ferr)?;
    lua.load(
        r#"
        local ok, err = pcall(ferr)
        assert(not ok and tostring(err):find("some async error"))
    "#,
    )
    .exec()
    .await
    .unwrap();

    Ok(())
}

#[tokio::test]
async fn test_async_function_wrap_raw() -> Result<()> {
    let lua = Luau::new();

    let f = Function::wrap_raw_async(|s: String| async {
        tokio::task::yield_now().await;
        s
    });
    lua.globals().set("f", f)?;
    let res: String = lua.load(r#"f("hello")"#).eval().await?;
    assert_eq!(res, "hello");

    // Return error
    let ferr = Function::wrap_raw_async(|| async {
        tokio::task::yield_now().await;
        Err::<(), _>("some error")
    });
    lua.globals().set("ferr", ferr)?;
    let (_, err): (Value, String) = lua.load(r#"ferr()"#).eval().await?;
    assert_eq!(err, "some error");

    Ok(())
}

#[tokio::test]
async fn test_async_sleep() -> Result<()> {
    let lua = Luau::new();

    let sleep = lua.create_async_function(async move |_lua, n: u64| {
        sleep_ms(n).await;
        Ok(format!("elapsed:{}ms", n))
    })?;
    lua.globals().set("sleep", sleep)?;

    let res: String = lua.load(r"return sleep(...)").call(100).await?;
    assert_eq!(res, "elapsed:100ms");

    Ok(())
}

#[tokio::test]
async fn test_async_call() -> Result<()> {
    let lua = Luau::new();

    let hello = lua.create_async_function(async |_lua, name: String| {
        sleep_ms(10).await;
        Ok(format!("hello, {}!", name))
    })?;

    hello.call::<()>("alex").await?;
    assert_eq!(hello.call::<String>("alex").await?, "hello, alex!");

    // Executing non-async functions using async call is allowed
    let sum = lua.create_function(|_lua, (a, b): (i64, i64)| Ok(a + b))?;
    assert_eq!(sum.call::<i64>((5, 1)).await?, 6);

    Ok(())
}

#[tokio::test]
async fn test_async_call_many_returns() -> Result<()> {
    let lua = Luau::new();

    let hello = lua.create_async_function(async |_lua, ()| {
        sleep_ms(10).await;
        Ok(("a", "b", "c", 1))
    })?;

    let vals = hello.call::<MultiValue>(()).await?;
    assert_eq!(vals.len(), 4);
    assert_eq!(vals[0].to_string()?, "a");
    assert_eq!(vals[1].to_string()?, "b");
    assert_eq!(vals[2].to_string()?, "c");
    assert_eq!(vals[3], Value::Integer(1));

    Ok(())
}

#[tokio::test]
async fn test_async_bind_call() -> Result<()> {
    let lua = Luau::new();

    let sum = lua.create_async_function(async |_lua, (a, b): (i64, i64)| {
        tokio::task::yield_now().await;
        Ok(a + b)
    })?;

    let plus_10 = sum.bind(10)?;
    lua.globals().set("plus_10", plus_10)?;

    assert_eq!(lua.load("plus_10(-1)").eval::<i64>().await?, 9);
    assert_eq!(lua.load("plus_10(1)").eval::<i64>().await?, 11);

    Ok(())
}

#[tokio::test]
async fn test_async_handle_yield() -> Result<()> {
    let lua = Luau::new();

    let sum = lua.create_async_function(async |_lua, (a, b): (i64, i64)| {
        sleep_ms(10).await;
        Ok(a + b)
    })?;

    lua.globals().set("sleep_sum", sum)?;

    let res: String = lua
        .load(
            r#"
        sum = sleep_sum(6, 7)
        assert(sum == 13)
        coroutine.yield("in progress")
        return "done"
    "#,
        )
        .call(())
        .await?;

    assert_eq!(res, "done");

    let min = lua
        .load(
            r#"
        function (a, b)
            coroutine.yield("ignore me")
            if a < b then return a else return b end
        end
    "#,
        )
        .eval::<Function>()
        .await?;
    assert_eq!(min.call::<i64>((-1, 1)).await?, -1);

    Ok(())
}

#[tokio::test]
async fn test_async_multi_return_nil() -> Result<()> {
    let lua = Luau::new();
    lua.globals().set(
        "func",
        lua.create_async_function(async |_, _: ()| Ok((Option::<String>::None, "error")))?,
    )?;

    lua.load(
        r#"
        local ok, err = func()
        assert(err == "error")
    "#,
    )
    .exec()
    .await
}

#[tokio::test]
async fn test_async_return_async_closure() -> Result<()> {
    let lua = Luau::new();

    let f = lua.create_async_function(async |lua, a: i64| {
        sleep_ms(10).await;

        let g = lua.create_async_function(async move |_, b: i64| {
            sleep_ms(10).await;
            Ok(a + b)
        })?;

        Ok(g)
    })?;

    lua.globals().set("f", f)?;

    let res: i64 = lua
        .load("local g = f(1); return g(2) + g(3)")
        .call(())
        .await?;

    assert_eq!(res, 7);

    Ok(())
}

#[tokio::test]
async fn test_async_thread_stream() -> Result<()> {
    let lua = Luau::new();

    let thread = lua.create_thread(
        lua.load(
            r#"
            function (sum)
                for i = 1,10 do
                    sum = sum + i
                    coroutine.yield(sum)
                end
                return sum
            end
            "#,
        )
        .eval()
        .await?,
    )?;

    let mut stream = thread.into_async::<i64>(1)?;
    let mut sum = 0;
    while let Some(n) = stream.try_next().await? {
        sum += n;
    }

    assert_eq!(sum, 286);

    Ok(())
}

#[tokio::test]
async fn test_async_thread() -> Result<()> {
    let lua = Luau::new();

    let cnt = Arc::new(10); // sleep 10ms
    let cnt2 = cnt.clone();
    let f = lua.create_async_function(async move |_lua, ()| {
        let cnt3 = cnt2.clone();
        sleep_ms(*cnt3.as_ref()).await;
        Ok("done")
    })?;

    let res: String = lua.create_thread(f)?.into_async(())?.await?;

    assert_eq!(res, "done");

    assert_eq!(Arc::strong_count(&cnt), 2);
    lua.gc_collect()?; // thread_s is non-resumable and subject to garbage collection
    assert_eq!(Arc::strong_count(&cnt), 1);

    Ok(())
}

#[tokio::test]
async fn test_async_thread_capture() -> Result<()> {
    let lua = Luau::new();

    let f = lua.create_async_function(async move |_lua, v: Value| {
        tokio::task::yield_now().await;
        drop(v);
        Ok(())
    })?;

    let thread = lua.create_thread(f)?;
    // After first resume, `v: Value` is captured in the coroutine
    thread.resume::<()>("abc").unwrap();
    drop(thread);

    Ok(())
}

#[tokio::test]
async fn test_async_table_object_like() -> Result<()> {
    let options = LuauOptions::new().thread_pool_size(4);
    let lua = Luau::new_with(StdLib::ALL_SAFE, options)?;

    let table = lua.create_table()?;
    table.set("val", 10)?;

    let get_value = lua.create_async_function(async |_, table: Table| {
        sleep_ms(10).await;
        table.get::<i64>("val")
    })?;
    table.set("get_value", get_value)?;

    let set_value = lua.create_async_function(async |_, (table, n): (Table, i64)| {
        sleep_ms(10).await;
        table.set("val", n)
    })?;
    table.set("set_value", set_value)?;

    assert_eq!(table.call_method::<i64>("get_value", ()).await?, 10);
    table.call_method::<()>("set_value", 15).await?;
    assert_eq!(table.call_method::<i64>("get_value", ()).await?, 15);

    let metatable = lua.create_table()?;
    metatable.set(
        "__call",
        lua.create_async_function(async |_, table: Table| {
            sleep_ms(10).await;
            table.get::<i64>("val")
        })?,
    )?;
    table.set_metatable(Some(metatable))?;
    assert_eq!(table.call::<i64>(()).await.unwrap(), 15);

    match table.call_method::<()>("non_existent", ()).await {
        Err(Error::RuntimeError(err)) => {
            assert!(err.contains("attempt to call a nil value (function 'non_existent')"))
        }
        r => panic!("expected RuntimeError, got {r:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_async_thread_pool() -> Result<()> {
    let options = LuauOptions::new().thread_pool_size(4);
    let lua = Luau::new_with(StdLib::ALL_SAFE, options)?;

    let error_f = lua.create_async_function(async |_, ()| {
        sleep_ms(10).await;
        Err::<(), _>(Error::runtime("test"))
    })?;

    let sleep = lua.create_async_function(async |_, n| {
        sleep_ms(n).await;
        Ok(format!("elapsed:{}ms", n))
    })?;

    assert!(error_f.call::<()>(()).await.is_err());
    // Next call should use cached thread
    assert_eq!(sleep.call::<String>(3).await?, "elapsed:3ms");

    Ok(())
}

#[tokio::test]
async fn test_async_userdata() -> Result<()> {
    struct MyUserdata(u64);

    impl UserData for MyUserdata {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_async_method("get_value", async |_, data, ()| {
                sleep_ms(10).await;
                Ok(data.0)
            });

            methods.add_async_method_mut("set_value", async |_, mut data, n| {
                sleep_ms(10).await;
                data.0 = n;
                Ok(())
            });

            methods.add_async_method_once("take_value", async |_, data, ()| {
                sleep_ms(10).await;
                Ok(data.0)
            });

            methods.add_async_function("sleep", async |_, n| {
                sleep_ms(n).await;
                Ok(format!("elapsed:{}ms", n))
            });
        }
    }

    let lua = Luau::new();
    let globals = lua.globals();

    let userdata = lua.create_userdata(MyUserdata(11))?;
    globals.set("userdata", &userdata)?;

    lua.load(
        r#"
        assert(userdata:get_value() == 11)
        userdata:set_value(12)
        assert(userdata.sleep(5) == "elapsed:5ms")
        assert(userdata:get_value() == 12)
    "#,
    )
    .exec()
    .await?;

    // ObjectLike methods
    userdata.call_method::<()>("set_value", 24).await?;
    let n: u64 = userdata.call_method("get_value", ()).await?;
    assert_eq!(n, 24);
    userdata.call_function::<()>("sleep", 15).await?;

    // Take value
    let userdata2 = lua.create_userdata(MyUserdata(0))?;
    globals.set("userdata2", userdata2)?;
    lua.load("assert(userdata:take_value() == 24)")
        .exec()
        .await?;
    match lua.load("userdata2.take_value(userdata)").exec().await {
        Err(Error::CallbackError { cause, .. }) => {
            let err = cause.to_string();
            assert!(err.contains("bad argument `self` to `MyUserdata.take_value`"));
            assert!(err.contains("userdata has been destructed"));
        }
        r => panic!("expected Err(CallbackError), got {r:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_async_thread_error() -> Result<()> {
    struct MyUserData;

    impl UserData for MyUserData {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_meta_method("__tostring", |_, _this, ()| Ok("myuserdata error"))
        }
    }

    let lua = Luau::new();
    let result = lua
        .load("function x(...) error(...) end x(...)")
        .set_name("chunk")
        .call::<()>(MyUserData)
        .await;
    assert!(
        matches!(result, Err(Error::RuntimeError(cause)) if cause.contains("myuserdata error")),
        "improper error traceback from dead thread"
    );

    Ok(())
}

#[tokio::test]
async fn test_async_terminate() -> Result<()> {
    // Future is dropped together with its Luau state.
    let mutex = Arc::new(Mutex::new(0u32));
    {
        let lua = Luau::new();
        let mutex2 = mutex.clone();
        let func = lua.create_async_function(async move |_, ()| {
            let mutex = mutex2.clone();
            let _guard = mutex.lock().await;
            sleep_ms(100).await;
            Ok(())
        })?;

        let _ = tokio::time::timeout(Duration::from_millis(30), func.call::<()>(())).await;
    }
    assert!(mutex.try_lock().is_ok());

    // Future is dropped, but `Luau` instance is still alive
    let lua = Luau::new();
    let func = lua.create_async_function(async move |_, mutex: UserDataRef<Arc<Mutex<u32>>>| {
        let _guard = mutex.lock().await;
        sleep_ms(100).await;
        Ok(())
    })?;
    let mutex2 = lua.create_any_userdata(mutex.clone())?;
    let _ = tokio::time::timeout(Duration::from_millis(30), func.call::<()>(mutex2)).await;
    assert!(mutex.try_lock().is_ok());

    // Direct AsyncThread drops are also cancellation points, even when the thread is not recycled.
    let lua = Luau::new();
    let func = lua.create_async_function(async move |_, mutex: UserDataRef<Arc<Mutex<u32>>>| {
        let _guard = mutex.lock().await;
        sleep_ms(100).await;
        Ok(())
    })?;
    let mutex2 = lua.create_any_userdata(mutex.clone())?;
    let thread = lua.create_thread(func)?;
    let _ = tokio::time::timeout(Duration::from_millis(30), thread.into_async::<()>(mutex2)?).await;
    assert!(mutex.try_lock().is_ok());

    Ok(())
}

#[tokio::test]
async fn test_async_task() -> Result<()> {
    let lua = Luau::new();

    let delay = lua.create_function(|lua, (secs, f, args): (f32, Function, MultiValue)| {
        let thread = lua.create_thread(f)?;
        let thread2 = thread.clone().into_async::<()>(args)?;
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_secs_f32(secs)).await;
            _ = thread2.await;
        });
        Ok(thread)
    })?;

    lua.globals().set("delay", delay)?;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            _ = lua
                .load("delay(0.1, function(msg) global_msg = msg end, 'done')")
                .exec()
                .await;
        })
        .await;
    local.await;
    assert_eq!(lua.globals().get::<String>("global_msg")?, "done");

    Ok(())
}

#[tokio::test]
async fn test_async_task_abort() -> Result<()> {
    let lua = Luau::new();

    let sleep = lua.create_async_function(async move |_lua, n: u64| {
        sleep_ms(n).await;
        Ok(())
    })?;
    lua.globals().set("sleep", sleep)?;

    let fut = lua.load("sleep(200) result = 'done'").exec();
    let _ = tokio::time::timeout(Duration::from_millis(100), fut).await;
    assert_eq!(lua.globals().get::<Value>("result")?, Value::Nil);

    Ok(())
}

#[tokio::test]
async fn test_async_yield_with() -> Result<()> {
    let lua = Luau::new();

    let func = lua.create_async_function(async |lua, (mut a, mut b): (i32, i32)| {
        let zero = lua.yield_with::<MultiValue>(()).await?;
        assert!(zero.is_empty());
        let one = lua.yield_with::<MultiValue>(a + b).await?;
        assert_eq!(one.len(), 1);

        for _ in 0..3 {
            (a, b) = lua.yield_with((a + b, a * b)).await?;
        }
        Ok((0, 0))
    })?;

    let thread = lua.create_thread(func)?;

    let zero = thread.resume::<MultiValue>((2, 3))?; // function arguments
    assert!(zero.is_empty());
    let one = thread.resume::<i32>(())?; // value of "zero" is passed here
    assert_eq!(one, 5);

    assert_eq!(thread.resume::<(i32, i32)>(1)?, (5, 6)); // value of "one" is passed here
    assert_eq!(thread.resume::<(i32, i32)>((10, 11))?, (21, 110));
    assert_eq!(thread.resume::<(i32, i32)>((11, 12))?, (23, 132));
    assert_eq!(thread.resume::<(i32, i32)>((12, 13))?, (0, 0));
    assert!(thread.is_finished());

    Ok(())
}
