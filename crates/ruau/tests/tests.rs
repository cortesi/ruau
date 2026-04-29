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

#[cfg(not(target_arch = "wasm32"))]
use std::iter::FromIterator;
use std::{
    collections::HashMap,
    error, f32, f64, fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use ruau::{
    Error, ExternalError, FromLuauMulti, Function, IntoLuauMulti, Luau, LuauOptions, Nil, Result,
    StdLib, Table, UserData, Value, Variadic,
};

fn call_sync<R>(lua: &Luau, function: Function, args: impl IntoLuauMulti) -> Result<R>
where
    R: FromLuauMulti,
{
    lua.create_thread(function)?.resume(args)
}

fn exec_sync(lua: &Luau, source: &str) -> Result<()> {
    call_sync(lua, lua.load(source).into_function()?, ())
}

fn call_chunk_sync<R>(lua: &Luau, source: &str, args: impl IntoLuauMulti) -> Result<R>
where
    R: FromLuauMulti,
{
    call_sync(lua, lua.load(source).into_function()?, args)
}

#[tokio::test]
async fn test_weak_lua() {
    let lua = Luau::new();
    let weak_lua = lua.weak();
    assert!(weak_lua.is_alive());
    drop(lua);
    assert!(!weak_lua.is_alive());
}

#[tokio::test]
async fn test_load() -> Result<()> {
    let lua = Luau::new();

    let func = lua.load("\treturn 1+2").into_function()?;
    let result: i32 = func.call(()).await?;
    assert_eq!(result, 3);

    assert!(lua.load("").exec().await.is_ok());
    assert!(lua.load("§$%§&$%&").exec().await.is_err());

    Ok(())
}

#[tokio::test]
async fn test_exec() -> Result<()> {
    let lua = Luau::new();

    let globals = lua.globals();
    lua.load(
        r#"
        res = 'foo'..'bar'
    "#,
    )
    .exec()
    .await?;
    assert_eq!(globals.get::<String>("res")?, "foobar");

    let module: Table = lua
        .load(
            r#"
            local module = {}

            function module.func()
                return "hello"
            end

            return module
        "#,
        )
        .eval()
        .await?;
    assert!(module.contains_key("func")?);
    assert_eq!(
        module.get::<Function>("func")?.call::<String>(()).await?,
        "hello"
    );

    Ok(())
}

#[tokio::test]
async fn test_eval() -> Result<()> {
    let lua = Luau::new();

    assert_eq!(lua.load("1 + 1").eval::<i32>().await?, 2);
    assert!(lua.load("false == false").eval::<bool>().await?);
    assert_eq!(lua.load("return 1 + 2").eval::<i32>().await?, 3);
    match lua.load("if true then").eval::<()>().await {
        Err(Error::SyntaxError {
            incomplete_input: true,
            ..
        }) => {}
        r => panic!(
            "expected SyntaxError with incomplete_input=true, got {:?}",
            r
        ),
    }

    Ok(())
}

#[tokio::test]
async fn test_replace_globals() -> Result<()> {
    let lua = Luau::new();

    let globals = lua.create_table()?;
    globals.set("foo", "bar")?;

    lua.set_globals(globals.clone())?;
    let val = lua.load("return foo").eval::<String>().await?;
    assert_eq!(val, "bar");

    // Updating globals in sandboxed Luau state is not allowed
    {
        lua.sandbox(true)?;
        match lua.set_globals(globals) {
            Err(Error::RuntimeError(msg))
                if msg.contains("cannot change globals in a sandboxed Luau state") => {}
            r => panic!("expected RuntimeError(...) with a specific error message, got {r:?}"),
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_load_mode() -> Result<()> {
    let lua = Luau::new();

    assert_eq!(
        lua.load("1 + 1")
            .set_text_mode()
            .eval::<i32>()
            .await?,
        2
    );
    match unsafe { lua.load("1 + 1").set_binary_mode() }.exec().await {
        Ok(_) => panic!("expected SyntaxError, got no error"),
        Err(Error::SyntaxError { message: msg, .. }) => {
            assert!(msg.contains("attempt to load a text chunk"))
        }
        Err(e) => panic!("expected SyntaxError, got {:?}", e),
    };

    let bytecode = ruau::Compiler::new().compile("return 1 + 1")?;
    assert_eq!(
        unsafe { lua.load(&bytecode).set_binary_mode() }
            .eval::<i32>()
            .await?,
        2
    );
    match lua.load(&bytecode).set_text_mode().exec().await {
        Ok(_) => panic!("expected SyntaxError, got no error"),
        Err(Error::SyntaxError { message: msg, .. }) => {
            assert!(msg.contains("attempt to load a binary chunk"))
        }
        Err(e) => panic!("expected SyntaxError, got {:?}", e),
    };

    Ok(())
}

#[tokio::test]
async fn test_lua_multi() -> Result<()> {
    let lua = Luau::new();

    lua.load(
        r#"
        function concat(arg1, arg2)
            return arg1 .. arg2
        end

        function mreturn()
            return 1, 2, 3, 4, 5, 6
        end
    "#,
    )
    .exec()
    .await?;

    let globals = lua.globals();
    let concat = globals.get::<Function>("concat")?;
    let mreturn = globals.get::<Function>("mreturn")?;

    assert_eq!(concat.call::<String>(("foo", "bar")).await?, "foobar");
    let (a, b) = mreturn.call::<(u64, u64)>(()).await?;
    assert_eq!((a, b), (1, 2));
    let (a, b, v) = mreturn.call::<(u64, u64, Variadic<u64>)>(()).await?;
    assert_eq!((a, b), (1, 2));
    assert_eq!(v[..], [3, 4, 5, 6]);

    Ok(())
}

#[tokio::test]
async fn test_coercion() -> Result<()> {
    let lua = Luau::new();

    lua.load(
        r#"
        int = 123
        str = "123"
        num = 123.0
        func = function() end
    "#,
    )
    .exec()
    .await?;

    let globals = lua.globals();
    assert_eq!(globals.get::<String>("int")?, "123");
    assert_eq!(globals.get::<i32>("str")?, 123);
    assert_eq!(globals.get::<i32>("num")?, 123);
    assert!(globals.get::<String>("func").is_err());

    Ok(())
}

#[tokio::test]
async fn test_error() -> Result<()> {
    #[derive(Debug)]
    pub struct TestError;

    impl fmt::Display for TestError {
        fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
            write!(fmt, "test error")
        }
    }

    impl error::Error for TestError {}

    let lua = Luau::new();

    let globals = lua.globals();
    lua.load(
        r#"
        function no_error()
        end

        function luau_error()
            error("this is a Luau error")
        end

        function rust_error()
            rust_error_function()
        end

        function return_error()
            local status, res = pcall(rust_error_function)
            assert(not status)
            return res
        end

        function return_string_error()
            return "this should be converted to an error"
        end

        function test_pcall()
            local testvar = 0

            pcall(function(arg)
                testvar = testvar + arg
                error("should be ignored")
            end, 3)

            local function handler(err)
                if string.match(_VERSION, "Luau") then
                    -- Luau includes the numeric error after a colon.
                    local caps = string.match(err, ': (%d+)$')
                    if caps then
                        err = caps
                    end
                end
                testvar = testvar + err
                return "should be ignored"
            end

            local status, res = xpcall(function()
                error(5)
            end, handler)
            assert(not status)

            if testvar ~= 8 then
                error("testvar had the wrong value, pcall / xpcall misbehaving "..testvar)
            end
        end

        function understand_recursion()
            understand_recursion()
        end
    "#,
    )
    .exec()
    .await?;

    let rust_error_function =
        lua.create_function(|_, ()| -> Result<()> { Err(TestError.into_luau_err()) })?;
    globals.set("rust_error_function", rust_error_function)?;

    let no_error = globals.get::<Function>("no_error")?;
    assert!(no_error.call::<()>(()).await.is_ok());

    let luau_error = globals.get::<Function>("luau_error")?;
    match luau_error.call::<()>(()).await {
        Err(Error::RuntimeError(_)) => {}
        Err(e) => panic!("error is not RuntimeError kind, got {:?}", e),
        _ => panic!("error not returned"),
    }

    let rust_error = globals.get::<Function>("rust_error")?;
    match rust_error.call::<()>(()).await {
        Err(Error::CallbackError { .. }) => {}
        Err(e) => panic!("error is not CallbackError kind, got {:?}", e),
        _ => panic!("error not returned"),
    }

    let return_error = globals.get::<Function>("return_error")?;
    match return_error.call::<Value>(()).await {
        Ok(Value::Error(_)) => {}
        _ => panic!("Value::Error not returned"),
    }

    let return_string_error = globals.get::<Function>("return_string_error")?;
    assert!(return_string_error.call::<Error>(()).await.is_ok());

    match lua
        .load("if you are happy and you know it syntax error")
        .exec()
        .await
    {
        Err(Error::SyntaxError {
            incomplete_input: false,
            ..
        }) => {}
        Err(_) => panic!("error is not LuauSyntaxError::Syntax kind"),
        _ => panic!("error not returned"),
    }
    match lua.load("function i_will_finish_what_i()").exec().await {
        Err(Error::SyntaxError {
            incomplete_input: true,
            ..
        }) => {}
        Err(_) => panic!("error is not LuauSyntaxError::IncompleteStatement kind"),
        _ => panic!("error not returned"),
    }

    let test_pcall = globals.get::<Function>("test_pcall")?;
    test_pcall.call::<()>(()).await?;

    #[cfg(not(target_arch = "wasm32"))]
    {
        let understand_recursion = globals.get::<Function>("understand_recursion")?;
        assert!(understand_recursion.call::<()>(()).await.is_err());
    }

    Ok(())
}

#[tokio::test]
#[cfg(not(panic = "abort"))]
async fn test_panic() -> Result<()> {
    fn make_lua(options: LuauOptions) -> Result<Luau> {
        let lua = Luau::new_with(StdLib::ALL_SAFE, options)?;
        let rust_panic_function = lua.create_function(|_, msg: Option<String>| -> Result<()> {
            if let Some(msg) = msg {
                panic!("{}", msg)
            }
            panic!("rust panic")
        })?;
        lua.globals()
            .set("rust_panic_function", rust_panic_function)?;
        Ok(lua)
    }

    // Test triggering Luau error with sending Rust panic (must be resumed)
    {
        let lua = make_lua(LuauOptions::default())?;

        match catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            exec_sync(
                &lua,
                r#"
                _, err = pcall(rust_panic_function)
                error(err)
            "#,
            )
        })) {
            Ok(Ok(_)) => panic!("no panic was detected"),
            Ok(Err(e)) => panic!("error during panic test {:?}", e),
            Err(p) => assert!(*p.downcast::<&str>().unwrap() == "rust panic"),
        };

        // Trigger same panic again
        match lua.load("error(err)").exec().await {
            Ok(_) => panic!("no error was detected"),
            Err(Error::PreviouslyResumedPanic) => {}
            Err(e) => panic!("expected PreviouslyResumedPanic, got {:?}", e),
        }
    }

    // Test returning Rust panic (must be resumed)
    {
        let lua = make_lua(LuauOptions::default())?;
        if let Ok(_) = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let _caught_panic = call_chunk_sync::<Value>(
                &lua,
                r#"
                    -- Set global
                    _, err = pcall(rust_panic_function)
                    return err
                "#,
                (),
            )?;
            Ok(())
        })) {
            panic!("no panic was detected")
        };

        assert!(lua.globals().get::<Value>("err")? == Value::Nil);
        match lua.load("tostring(err)").exec().await {
            Ok(_) => panic!("no error was detected"),
            Err(Error::CallbackError { ref cause, .. }) => match cause.as_ref() {
                Error::PreviouslyResumedPanic => {}
                e => panic!("expected PreviouslyResumedPanic, got {:?}", e),
            },
            Err(e) => panic!("expected CallbackError, got {:?}", e),
        }
    }

    // Test representing Rust panic as a string
    match catch_unwind(|| -> Result<()> {
        let lua = make_lua(LuauOptions::default())?;
        exec_sync(
            &lua,
            r#"
            local _, err = pcall(rust_panic_function)
            error(tostring(err))
        "#,
        )
    }) {
        Ok(Ok(_)) => panic!("no error was detected"),
        Ok(Err(Error::RuntimeError(_))) => {}
        Ok(Err(e)) => panic!("expected RuntimeError, got {:?}", e),
        Err(_) => panic!("panic was detected"),
    }

    // Test disabling `catch_rust_panics` option / pcall correctness
    match catch_unwind(|| -> Result<()> {
        let lua = make_lua(LuauOptions::new().catch_rust_panics(false))?;
        exec_sync(
            &lua,
            r#"
            local ok, err = pcall(function(msg) error(msg) end, "hello")
            assert(not ok and err:find("hello") ~= nil)

            ok, err = pcall(rust_panic_function, "rust panic from lua")
            -- Nothing to return, panic should be automatically resumed
        "#,
        )
    }) {
        Ok(r) => panic!("no panic was detected: {:?}", r),
        Err(p) => assert!(*p.downcast::<String>().unwrap() == "rust panic from lua"),
    }

    // Test disabling `catch_rust_panics` option / xpcall correctness
    match catch_unwind(|| -> Result<()> {
        let lua = make_lua(LuauOptions::new().catch_rust_panics(false))?;
        exec_sync(
            &lua,
            r#"
            local msgh_ok = false
            local msgh = function(err)
                msgh_ok = err ~= nil and err:find("hello") ~= nil
                return err
            end
            local ok, err = xpcall(function(msg) error(msg) end, msgh, "hello")
            assert(not ok and err:find("hello") ~= nil)
            assert(msgh_ok)

            ok, err = xpcall(rust_panic_function, msgh, "rust panic from lua")
            -- Nothing to return, panic should be automatically resumed
        "#,
        )
    }) {
        Ok(r) => panic!("no panic was detected: {:?}", r),
        Err(p) => assert!(*p.downcast::<String>().unwrap() == "rust panic from lua"),
    }

    Ok(())
}

#[cfg(target_pointer_width = "64")]
#[tokio::test]
async fn test_safe_integers() -> Result<()> {
    const MAX_SAFE_INTEGER: i64 = 2i64.pow(53) - 1;
    const MIN_SAFE_INTEGER: i64 = -2i64.pow(53) + 1;

    let lua = Luau::new();
    let f = lua.load("return ...").into_function()?;

    assert_eq!(f.call::<i64>(MAX_SAFE_INTEGER).await?, MAX_SAFE_INTEGER);
    assert_eq!(f.call::<i64>(MIN_SAFE_INTEGER).await?, MIN_SAFE_INTEGER);

    // Luau converts values outside the safe integer range to f64.
    assert_ne!(
        f.call::<i64>(MAX_SAFE_INTEGER + 2).await?,
        MAX_SAFE_INTEGER + 2
    );
    assert_ne!(
        f.call::<i64>(MIN_SAFE_INTEGER - 2).await?,
        MIN_SAFE_INTEGER - 2
    );
    assert_eq!(f.call::<f64>(i64::MAX).await?, i64::MAX as f64);

    Ok(())
}

#[tokio::test]
async fn test_num_conversion() -> Result<()> {
    let lua = Luau::new();

    assert_eq!(
        lua.coerce_integer(Value::String(lua.create_string("1")?))?,
        Some(1)
    );
    assert_eq!(
        lua.coerce_integer(Value::String(lua.create_string("1.0")?))?,
        Some(1)
    );
    assert_eq!(
        lua.coerce_integer(Value::String(lua.create_string("1.5")?))?,
        None
    );

    assert_eq!(
        lua.coerce_number(Value::String(lua.create_string("1")?))?,
        Some(1.0)
    );
    assert_eq!(
        lua.coerce_number(Value::String(lua.create_string("1.0")?))?,
        Some(1.0)
    );
    assert_eq!(
        lua.coerce_number(Value::String(lua.create_string("1.5")?))?,
        Some(1.5)
    );

    assert_eq!(lua.load("1.0").eval::<i64>().await?, 1);
    assert_eq!(lua.load("1.0").eval::<f64>().await?, 1.0);
    assert_eq!(lua.load("1.0").eval::<String>().await?, "1");

    assert_eq!(lua.load("1.5").eval::<i64>().await?, 1);
    assert_eq!(lua.load("1.5").eval::<f64>().await?, 1.5);
    assert_eq!(lua.load("1.5").eval::<String>().await?, "1.5");

    assert!(lua.load("-1").eval::<u64>().await.is_err());
    assert_eq!(lua.load("-1").eval::<i64>().await?, -1);

    assert!(lua.unpack::<u64>(lua.pack(1u128 << 64)?).is_err());
    assert!(lua.load("math.huge").eval::<i64>().await.is_err());

    assert_eq!(lua.unpack::<f64>(lua.pack(f32::MAX)?)?, f32::MAX as f64);
    assert_eq!(lua.unpack::<f64>(lua.pack(f32::MIN)?)?, f32::MIN as f64);
    assert_eq!(lua.unpack::<f32>(lua.pack(f64::MAX)?)?, f32::INFINITY);
    assert_eq!(lua.unpack::<f32>(lua.pack(f64::MIN)?)?, f32::NEG_INFINITY);

    assert_eq!(lua.unpack::<i128>(lua.pack(1i128 << 64)?)?, 1i128 << 64);

    // Negative zero
    let negative_zero = lua.load("-0.0").eval::<f64>().await?;
    assert_eq!(negative_zero, 0.0);
    assert!(negative_zero.is_sign_negative());

    let negative_zero = lua.load("-0").eval::<f64>().await?;
    assert_eq!(negative_zero, 0.0);
    assert!(negative_zero.is_sign_negative());

    Ok(())
}

#[tokio::test]
async fn test_pcall_xpcall() -> Result<()> {
    let lua = Luau::new();
    let globals = lua.globals();

    // make sure that we handle not enough arguments

    assert!(lua.load("pcall()").exec().await.is_err());
    assert!(lua.load("xpcall()").exec().await.is_err());
    assert!(lua.load("xpcall(function() end)").exec().await.is_err());

    // Make sure that the return values from are correct on success

    let (r, e) = lua
        .load("pcall(function(p) return p end, 'foo')")
        .eval::<(bool, String)>()
        .await?;
    assert!(r);
    assert_eq!(e, "foo");

    let (r, e) = lua
        .load("xpcall(function(p) return p end, print, 'foo')")
        .eval::<(bool, String)>()
        .await?;
    assert!(r);
    assert_eq!(e, "foo");

    // Make sure that the return values are correct on errors, and that error handling works

    lua.load(
        r#"
        pcall_error = nil
        pcall_status, pcall_error = pcall(error, "testerror")

        xpcall_error = nil
        xpcall_status, _ = xpcall(error, function(err) xpcall_error = err end, "testerror")
    "#,
    )
    .exec()
    .await?;

    assert!(!globals.get::<bool>("pcall_status")?);
    assert_eq!(globals.get::<String>("pcall_error")?, "testerror");

    assert!(!globals.get::<bool>("xpcall_statusr")?);
    assert_eq!(
        globals.get::<std::string::String>("xpcall_error")?,
        "testerror"
    );

    // Make sure that weird xpcall error recursion at least doesn't cause unsafety or panics.
    lua.load(
        r#"
        function xpcall_recursion()
            xpcall(error, function(err) error(err) end, "testerror")
        end
    "#,
    )
    .exec()
    .await?;
    globals
        .get::<Function>("xpcall_recursion")?
        .call::<()>(())
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_recursive_mut_callback_error() -> Result<()> {
    let lua = Luau::new();

    let mut v = Some(Box::new(123));
    let f = lua.create_function_mut(move |lua, mutate: bool| {
        if mutate {
            v = None;
        } else {
            // Produce a mutable reference
            let r = v.as_mut().unwrap();
            // Whoops, this will recurse into the function and produce another mutable reference!
            call_sync::<()>(lua, lua.globals().get::<Function>("f")?, true)?;
            println!("Should not get here, mutable aliasing has occurred!");
            println!("value at {:p} is {r}", r as *mut _);
        }

        Ok(())
    })?;
    lua.globals().set("f", f)?;
    match lua.globals().get::<Function>("f")?.call::<()>(false).await {
        Err(Error::CallbackError { ref cause, .. }) => match *cause.as_ref() {
            Error::CallbackError { ref cause, .. } => match *cause.as_ref() {
                Error::RecursiveMutCallback => {}
                ref other => panic!("incorrect result: {:?}", other),
            },
            ref other => panic!("incorrect result: {:?}", other),
        },
        other => panic!("incorrect result: {:?}", other),
    };

    Ok(())
}

#[tokio::test]
async fn test_set_metatable_nil() -> Result<()> {
    let lua = Luau::new();
    lua.load(
        r#"
        a = {}
        setmetatable(a, nil)
    "#,
    )
    .exec()
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_named_registry_value() -> Result<()> {
    let lua = Luau::new();

    lua.set_named_registry_value("test", 42)?;
    let f = lua.create_function(move |lua, ()| {
        assert_eq!(lua.named_registry_value::<i32>("test")?, 42);
        Ok(())
    })?;

    f.call::<()>(()).await?;

    lua.unset_named_registry_value("test")?;
    match lua.named_registry_value("test")? {
        Nil => {}
        val => panic!("registry value was not Nil, was {:?}", val),
    };

    Ok(())
}

#[tokio::test]
async fn test_registry_value() -> Result<()> {
    let lua = Luau::new();

    let mut r = Some(lua.create_registry_value(42)?);
    let f = lua.create_function_mut(move |lua, ()| {
        if let Some(r) = r.take() {
            assert_eq!(lua.registry_value::<i32>(&r)?, 42);
            lua.remove_registry_value(r).unwrap();
        } else {
            panic!();
        }
        Ok(())
    })?;

    f.call::<()>(()).await?;

    Ok(())
}

#[tokio::test]
async fn test_drop_registry_value() -> Result<()> {
    struct MyUserdata(#[allow(unused)] Arc<()>);

    impl UserData for MyUserdata {}

    let lua = Luau::new();
    let rc = Arc::new(());

    let r = lua.create_registry_value(MyUserdata(rc.clone()))?;
    assert_eq!(Arc::strong_count(&rc), 2);

    drop(r);
    lua.expire_registry_values();

    lua.load(r#"collectgarbage("collect")"#).exec().await?;

    assert_eq!(Arc::strong_count(&rc), 1);

    Ok(())
}

#[tokio::test]
async fn test_replace_registry_value() -> Result<()> {
    let lua = Luau::new();

    let mut key = lua.create_registry_value(42)?;
    lua.replace_registry_value(&mut key, "new value")?;
    assert_eq!(lua.registry_value::<String>(&key)?, "new value");
    lua.replace_registry_value(&mut key, Value::Nil)?;
    assert_eq!(lua.registry_value::<Value>(&key)?, Value::Nil);
    lua.replace_registry_value(&mut key, 123)?;
    assert_eq!(lua.registry_value::<i32>(&key)?, 123);

    let mut key2 = lua.create_registry_value(Value::Nil)?;
    lua.replace_registry_value(&mut key2, Value::Nil)?;
    assert_eq!(lua.registry_value::<Value>(&key2)?, Value::Nil);
    lua.replace_registry_value(&mut key2, "abc")?;
    assert_eq!(lua.registry_value::<String>(&key2)?, "abc");

    Ok(())
}

#[tokio::test]
async fn test_lua_registry_hash() -> Result<()> {
    let lua = Luau::new();

    let r1 = Arc::new(lua.create_registry_value("value1")?);
    let r2 = Arc::new(lua.create_registry_value("value2")?);

    let mut map = HashMap::new();
    map.insert(r1.clone(), "value1");
    map.insert(r2.clone(), "value2");

    assert_eq!(map[&r1], "value1");
    assert_eq!(map[&r2], "value2");

    Ok(())
}

#[tokio::test]
async fn test_lua_registry_ownership() -> Result<()> {
    let lua1 = Luau::new();
    let lua2 = Luau::new();

    let r1 = lua1.create_registry_value("hello")?;
    let r2 = lua2.create_registry_value("hello")?;

    assert!(lua1.owns_registry_value(&r1));
    assert!(!lua2.owns_registry_value(&r1));
    assert!(lua2.owns_registry_value(&r2));
    assert!(!lua1.owns_registry_value(&r2));

    Ok(())
}

#[tokio::test]
async fn test_mismatched_registry_key() -> Result<()> {
    let lua1 = Luau::new();
    let lua2 = Luau::new();

    let r = lua1.create_registry_value("hello")?;
    match lua2.remove_registry_value(r) {
        Err(Error::MismatchedRegistryKey) => {}
        r => panic!("wrong result type for mismatched registry key, {:?}", r),
    };

    Ok(())
}

#[tokio::test]
async fn test_registry_value_reuse() -> Result<()> {
    let lua = Luau::new();

    let r1 = lua.create_registry_value("value1")?;
    let r1_slot = format!("{r1:?}");
    drop(r1);

    // Previous slot must not be reused by nil value
    let r2 = lua.create_registry_value(Value::Nil)?;
    let r2_slot = format!("{r2:?}");
    assert_ne!(r1_slot, r2_slot);
    drop(r2);

    // But should be reused by non-nil value
    let r3 = lua.create_registry_value("value3")?;
    let r3_slot = format!("{r3:?}");
    assert_eq!(r1_slot, r3_slot);

    Ok(())
}

#[tokio::test]
#[cfg(not(panic = "abort"))]
async fn test_application_data() -> Result<()> {
    let lua = Luau::new();

    lua.set_app_data("test1");
    lua.set_app_data(vec!["test2"]);

    // Borrow &str immutably and Vec<&str> mutably
    let s = lua.app_data_ref::<&str>().unwrap();
    let mut v = lua.app_data_mut::<Vec<&str>>().unwrap();
    v.push("test3");

    // Insert of new data or removal should fail now
    assert!(lua.try_set_app_data::<i32>(123).is_err());
    if catch_unwind(AssertUnwindSafe(|| lua.set_app_data::<i32>(123))).is_ok() {
        panic!("expected panic")
    }
    if catch_unwind(AssertUnwindSafe(|| lua.remove_app_data::<i32>())).is_ok() {
        panic!("expected panic")
    }

    // Check display and debug impls
    assert_eq!(format!("{s}"), "test1");
    assert_eq!(format!("{s:?}"), "\"test1\"");

    // Borrowing immutably and mutably of the same type is not allowed
    assert!(lua.try_app_data_mut::<&str>().is_err());
    if let Ok(_) = catch_unwind(AssertUnwindSafe(|| lua.app_data_mut::<&str>().unwrap())) {
        panic!("expected panic")
    }
    assert!(lua.try_app_data_ref::<Vec<&str>>().is_err());
    drop((s, v));

    // Test that application data is accessible from anywhere
    let f = lua.create_function(|lua, ()| {
        let mut data1 = lua.app_data_mut::<&str>().unwrap();
        assert_eq!(*data1, "test1");
        *data1 = "test4";

        let data2 = lua.app_data_ref::<Vec<&str>>().unwrap();
        assert_eq!(*data2, vec!["test2", "test3"]);

        Ok(())
    })?;
    f.call::<()>(()).await?;

    assert_eq!(*lua.app_data_ref::<&str>().unwrap(), "test4");
    assert_eq!(
        *lua.app_data_ref::<Vec<&str>>().unwrap(),
        vec!["test2", "test3"]
    );

    lua.remove_app_data::<Vec<&str>>();
    assert!(lua.app_data_ref::<Vec<&str>>().is_none());

    Ok(())
}

#[tokio::test]
async fn test_rust_function() -> Result<()> {
    let lua = Luau::new();

    let globals = lua.globals();
    lua.load(
        r#"
        function lua_function()
            return rust_function()
        end

        -- Test to make sure chunk return is ignored
        return 1
    "#,
    )
    .exec()
    .await?;

    let lua_function = globals.get::<Function>("lua_function")?;
    let rust_function = lua.create_function(|_, ()| Ok("hello"))?;

    globals.set("rust_function", rust_function)?;
    assert_eq!(lua_function.call::<String>(()).await?, "hello");

    Ok(())
}

#[tokio::test]
#[cfg(not(target_arch = "wasm32"))]
async fn test_recursion() -> Result<()> {
    let lua = Luau::new();

    let f = lua.create_function(move |lua, i: i32| {
        if i < 64 {
            call_sync::<()>(lua, lua.globals().get::<Function>("f")?, i + 1)?;
        }
        Ok(())
    })?;

    lua.globals().set("f", &f)?;
    f.call::<()>(1).await?;

    Ok(())
}

#[tokio::test]
#[cfg(not(target_arch = "wasm32"))]
async fn test_too_many_returns() -> Result<()> {
    let lua = Luau::new();
    let f = lua.create_function(|_, ()| Ok(Variadic::from_iter(1..1000000)))?;
    assert!(f.call::<Variadic<u32>>(()).await.is_err());
    Ok(())
}

#[tokio::test]
#[cfg(not(target_arch = "wasm32"))]
async fn test_too_many_arguments() -> Result<()> {
    let lua = Luau::new();
    lua.load("function test(...) end").exec().await?;
    let args = Variadic::from_iter(1..1000000);
    assert!(lua.globals().get::<Function>("test")?.bind(args).is_err());
    Ok(())
}

#[tokio::test]
#[cfg(not(target_arch = "wasm32"))]
async fn test_too_many_recursions() -> Result<()> {
    let lua = Luau::new();

    let f = lua.create_function(move |lua, ()| {
        call_sync::<()>(lua, lua.globals().get::<Function>("f")?, ())
    })?;

    lua.globals().set("f", &f)?;
    assert!(f.call::<()>(()).await.is_err());

    Ok(())
}

#[tokio::test]
#[cfg(not(target_arch = "wasm32"))]
async fn test_ref_stack_exhaustion() {
    match catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        let lua = Luau::new();
        let mut vals = Vec::new();
        for _ in 0..10000000 {
            vals.push(lua.create_table()?);
        }
        Ok(())
    })) {
        Ok(_) => panic!("no panic was detected"),
        Err(p) => assert!(
            p.downcast::<String>()
                .unwrap()
                .starts_with("cannot create a Luau reference, out of auxiliary stack space")
        ),
    }
}

#[tokio::test]
async fn test_large_args() -> Result<()> {
    let lua = Luau::new();
    let globals = lua.globals();

    globals.set(
        "c",
        lua.create_function(|_, args: Variadic<usize>| {
            let mut s = 0;
            for i in 0..args.len() {
                s += i;
                assert_eq!(i, args[i]);
            }
            Ok(s)
        })?,
    )?;

    let f: Function = lua
        .load(
            r#"
            return function(...)
                return c(...)
            end
        "#,
        )
        .eval()
        .await?;

    assert_eq!(
        f.call::<usize>((0..100).collect::<Variadic<usize>>())
            .await?,
        4950
    );

    Ok(())
}

#[tokio::test]
async fn test_large_args_ref() -> Result<()> {
    let lua = Luau::new();

    let f = lua.create_function(|_, args: Variadic<String>| {
        for i in 0..args.len() {
            assert_eq!(args[i], i.to_string());
        }
        Ok(())
    })?;

    f.call::<()>((0..100).map(|i| i.to_string()).collect::<Variadic<_>>())
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_chunk_env() -> Result<()> {
    let lua = Luau::new();

    let assert: Function = lua.globals().get("assert")?;

    let env1 = lua.create_table()?;
    env1.set("assert", assert.clone())?;

    let env2 = lua.create_table()?;
    env2.set("assert", assert)?;

    lua.load(
        r#"
        test_var = 1
    "#,
    )
    .set_environment(env1.clone())
    .exec()
    .await?;

    lua.load(
        r#"
        assert(test_var == nil)
        test_var = 2
    "#,
    )
    .set_environment(env2.clone())
    .exec()
    .await?;

    assert_eq!(
        lua.load("test_var")
            .set_environment(env1)
            .eval::<i32>()
            .await?,
        1
    );
    assert_eq!(
        lua.load("test_var")
            .set_environment(env2)
            .eval::<i32>()
            .await?,
        2
    );

    Ok(())
}

#[tokio::test]
async fn test_context_thread() -> Result<()> {
    let lua = Luau::new();

    let f = lua
        .load(
            r#"
            local thread = coroutine.running()
            assert(coroutine.running() == thread)
        "#,
        )
        .into_function()?;

    f.call::<()>(Nil).await?;

    Ok(())
}

#[tokio::test]
async fn test_inspect_stack() -> Result<()> {
    let lua = Luau::new();

    // Not inside any function
    assert!(lua.inspect_stack(0, |_| ()).is_none());

    let logline = lua.create_function(|lua, msg: String| {
        let r = lua
            .inspect_stack(1, |debug| {
                let source = debug.source().short_src;
                let source = source.as_deref().unwrap_or("?");
                let line = debug.current_line().unwrap();
                format!("{}:{} {}", source, line, msg)
            })
            .unwrap();
        Ok(r)
    })?;
    lua.globals().set("logline", logline)?;

    lua.load(
        r#"
        local function foo()
            local line = logline("hello")
            return line
        end
        local function bar()
            return foo()
        end

        assert(foo() == '[string "chunk"]:3 hello')
        assert(bar() == '[string "chunk"]:3 hello')
        assert(logline("world") == '[string "chunk"]:12 world')
    "#,
    )
    .set_name("chunk")
    .exec()
    .await?;

    let stack_info = lua.create_function(|lua, ()| {
        let stack_info = lua.inspect_stack(1, |debug| debug.stack()).unwrap();
        Ok(format!("{stack_info:?}"))
    })?;
    lua.globals().set("stack_info", stack_info)?;

    lua.load(
        r#"
        local stack_info = stack_info
        local function baz(a, b, c, ...)
            return stack_info()
        end
        assert(baz() == 'DebugStack { num_upvalues: 1, num_params: 3, is_vararg: true }')
    "#,
    )
    .exec()
    .await?;

    // Test retrieving currently running function
    let running_function =
        lua.create_function(|lua, ()| Ok(lua.inspect_stack(1, |debug| debug.function())))?;
    lua.globals().set("running_function", running_function)?;
    lua.load(
        r#"
        local function baz()
            return running_function()
        end
        assert(baz() == baz)
    "#,
    )
    .exec()
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_traceback() -> Result<()> {
    let lua = Luau::new();

    // Test traceback at level 0 (not inside any function)
    let traceback = lua.traceback(None, 0)?.to_string_lossy();
    assert!(traceback.contains("stack traceback:"));

    // Test traceback with a message prefix
    let traceback = lua.traceback(Some("error occurred"), 0)?.to_string_lossy();
    assert!(traceback.starts_with("error occurred"));
    assert!(traceback.contains("stack traceback:"));

    // Test traceback inside a function
    let get_traceback = lua.create_function(|lua, (msg, level): (Option<String>, usize)| {
        lua.traceback(msg.as_deref(), level)
    })?;
    lua.globals().set("get_traceback", get_traceback)?;

    lua.load(
        r#"
        local function foo()
            -- Level 1 is inside foo (the caller)
            local traceback = get_traceback(nil, 1)
            return traceback
        end
        local function bar()
            local result = foo()
            return result
        end
        local function baz()
            local result = bar()
            return result
        end

        local traceback = baz()
        assert(traceback:match("in %a+ 'foo'"))
        assert(traceback:match("in %a+ 'bar'"))
        assert(traceback:match("in %a+ 'baz'"))
    "#,
    )
    .exec()
    .await?;

    // Test traceback at different levels
    lua.load(
        r#"
        local function foo()
            local tb0 = get_traceback(nil, 0)
            local tb1 = get_traceback(nil, 1)
            local tb2 = get_traceback(nil, 2)
            return tb0, tb1, tb2
        end
        local function bar()
            local tb0, tb1, tb2 = foo()
            return tb0, tb1, tb2
        end

        local tb0, tb1, tb2 = bar()

        assert(tb0:match("in %a+ 'get_traceback'"))
        assert(tb0:match("in %a+ 'foo'"))

        assert(not tb1:match("in %a+ 'get_traceback'"))
        assert(tb1:match("in %a+ 'foo'"))

        assert(not tb2:match("in %a+ 'foo'"))
        assert(tb1:match("in %a+ 'bar'"))
    "#,
    )
    .exec()
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_multi_states() -> Result<()> {
    let lua = Luau::new();

    let f = lua.create_function(|lua, g: Option<Function>| {
        if let Some(g) = g {
            call_sync::<()>(lua, g, ())?;
        }
        Ok(())
    })?;
    lua.globals().set("f", f)?;

    lua.load("f(function() coroutine.wrap(function() f() end)() end)")
        .exec()
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_gc_drop_ref_thread() -> Result<()> {
    let lua = Luau::new();

    let t = lua.create_table()?;
    lua.create_function(move |_, ()| {
        _ = &t;
        Ok(())
    })?;

    for _ in 0..10000 {
        // GC will run eventually to collect the function and the table above
        lua.create_table()?;
    }

    Ok(())
}
