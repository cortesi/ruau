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

use ruau::{CoverageInfo, Error, Function, Luau, Result, Table, Variadic};

#[tokio::test]
async fn test_function_call() -> Result<()> {
    let lua = Luau::new();

    let concat = lua
        .load(r#"function(arg1, arg2) return arg1 .. arg2 end"#)
        .eval::<Function>()
        .await?;
    assert_eq!(concat.call::<String>(("foo", "bar")).await?, "foobar");

    Ok(())
}

#[tokio::test]
async fn test_function_call_error() -> Result<()> {
    let lua = Luau::new();

    let concat_err = lua
        .load(r#"function(arg1, arg2) error("concat error") end"#)
        .eval::<Function>()
        .await?;
    match concat_err.call::<String>(("foo", "bar")).await {
        Err(Error::RuntimeError(msg)) if msg.contains("concat error") => {}
        other => panic!("unexpected result: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_function_bind() -> Result<()> {
    let lua = Luau::new();

    let globals = lua.globals();
    lua.load(
        r#"
        function concat(...)
            local res = ""
            for _, s in pairs({...}) do
                res = res..s
            end
            return res
        end
    "#,
    )
    .exec()
    .await?;

    let mut concat = globals.get::<Function>("concat")?;
    concat = concat.bind("foo")?;
    concat = concat.bind("bar")?;
    concat = concat.bind(("baz", "baf"))?;
    assert_eq!(concat.call::<String>(()).await?, "foobarbazbaf");
    assert_eq!(
        concat.call::<String>(("hi", "wut")).await?,
        "foobarbazbafhiwut"
    );

    let mut concat2 = globals.get::<Function>("concat")?;
    concat2 = concat2.bind(())?;
    assert_eq!(concat2.call::<String>(()).await?, "");
    assert_eq!(concat2.call::<String>(("ab", "cd")).await?, "abcd");

    Ok(())
}

#[tokio::test]
#[cfg(not(target_arch = "wasm32"))]
async fn test_function_bind_error() -> Result<()> {
    let lua = Luau::new();

    let func = lua.load(r#"function(...) end"#).eval::<Function>().await?;
    assert!(matches!(
        func.bind(Variadic::from_iter(1..1000000)),
        Err(Error::BindError)
    ));

    Ok(())
}

#[tokio::test]
async fn test_function_environment() -> Result<()> {
    let lua = Luau::new();
    let globals = lua.globals();

    // We must not get or set environment for C functions
    let rust_func = lua.create_function(|_, ()| Ok("hello"))?;
    assert_eq!(rust_func.environment(), None);
    assert_eq!(rust_func.set_environment(globals.clone()).ok(), Some(false));

    // Test getting Luau function environment
    globals.set("hello", "global")?;
    let lua_func = lua
        .load(
            r#"
        local t = ""
        return function()
            -- two upvalues
            return t .. hello
        end
    "#,
        )
        .eval::<Function>()
        .await?;
    let lua_func2 = lua.load("return hello").into_function()?;
    assert_eq!(lua_func.call::<String>(()).await?, "global");
    assert_eq!(lua_func.environment().as_ref(), Some(&globals));

    // Test changing the environment
    let env = lua.create_table_from([("hello", "local")])?;
    assert!(lua_func.set_environment(env.clone())?);
    assert_eq!(lua_func.call::<String>(()).await?, "local");
    assert_eq!(lua_func2.call::<String>(()).await?, "global");

    // More complex case
    lua.load(
        r#"
        local number = 15
        function lucky() return tostring("number is "..number) end
        new_env = {
            tostring = function() return tostring(number) end,
        }
    "#,
    )
    .exec()
    .await?;
    let lucky = globals.get::<Function>("lucky")?;
    assert_eq!(lucky.call::<String>(()).await?, "number is 15");
    let new_env = globals.get::<Table>("new_env")?;
    lucky.set_environment(new_env)?;
    assert_eq!(lucky.call::<String>(()).await?, "15");

    // Test inheritance
    let lua_func2 = lua
        .load(r#"return function() return (function() return hello end)() end"#)
        .eval::<Function>()
        .await?;
    assert!(lua_func2.set_environment(env)?);
    lua.gc_collect()?;
    assert_eq!(lua_func2.call::<String>(()).await?, "local");

    // Test getting environment set by chunk loader
    let chunk = lua
        .load("return hello")
        .environment(lua.create_table_from([("hello", "chunk")])?)
        .into_function()?;
    assert_eq!(
        chunk.environment().unwrap().get::<String>("hello")?,
        "chunk"
    );

    Ok(())
}

#[tokio::test]
async fn test_function_info() -> Result<()> {
    let lua = Luau::new();

    let globals = lua.globals();
    lua.load(
        r#"
        function function1()
            return function() end
        end
    "#,
    )
    .name("source1")
    .exec()
    .await?;

    let function1 = globals.get::<Function>("function1")?;
    let function2 = function1.call::<Function>(()).await?;
    let function3 = lua.create_function(|_, ()| Ok(()))?;

    let function1_info = function1.info();
    assert_eq!(function1_info.name.as_deref(), Some("function1"));
    assert_eq!(function1_info.source.as_deref(), Some("source1"));
    assert_eq!(function1_info.line_defined, Some(2));
    assert_eq!(function1_info.what, "Lua");

    let function2_info = function2.info();
    assert_eq!(function2_info.name, None);
    assert_eq!(function2_info.source.as_deref(), Some("source1"));
    assert_eq!(function2_info.line_defined, Some(3));
    assert_eq!(function2_info.what, "Lua");

    let function3_info = function3.info();
    assert_eq!(function3_info.name, None);
    assert_eq!(function3_info.source.as_deref(), Some("=[C]"));
    assert_eq!(function3_info.line_defined, None);
    assert_eq!(function3_info.what, "C");

    let print_info = globals.get::<Function>("print")?.info();
    assert_eq!(print_info.name.as_deref(), Some("print"));
    assert_eq!(print_info.source.as_deref(), Some("=[C]"));
    assert_eq!(print_info.what, "C");
    assert_eq!(print_info.line_defined, None);

    // Function with upvalues and params
    let func_with_upvalues = lua
        .load(
            r#"
        local x, y = ...
        return function(a, ...)
            return a*x + y
        end
    "#,
        )
        .call::<Function>((10, 20))
        .await?;
    let func_with_upvalues_info = func_with_upvalues.info();
    assert_eq!(func_with_upvalues_info.num_upvalues, 2);
    assert_eq!(func_with_upvalues_info.num_params, 1);
    assert!(func_with_upvalues_info.is_vararg);

    Ok(())
}

#[tokio::test]
async fn test_function_coverage() -> Result<()> {
    let lua = Luau::new();

    lua.set_compiler(ruau::Compiler::default().coverage_level(ruau::CoverageLevel::Statement));

    let f = lua
        .load(
            r#"local s = "abc"
        assert(#s == 3)

        function abc(i)
            if i < 5 then
                return 0
            else
                return 1
            end
        end

        (function()
            (function() abc(10) end)()
        end)()
        "#,
        )
        .into_function()?;

    f.call::<()>(()).await?;

    let mut report = Vec::new();
    f.coverage(|cov| {
        report.push(cov);
    });

    assert_eq!(
        report[0],
        CoverageInfo {
            function: None,
            line_defined: 1,
            depth: 0,
            hits: vec![-1, 1, 1, -1, 1, -1, -1, -1, -1, -1, -1, -1, 1, -1, -1, -1],
        }
    );
    assert_eq!(
        report[1],
        CoverageInfo {
            function: Some("abc".into()),
            line_defined: 4,
            depth: 1,
            hits: vec![-1, -1, -1, -1, -1, 1, 0, -1, 1, -1, -1, -1, -1, -1, -1, -1],
        }
    );
    assert_eq!(
        report[2],
        CoverageInfo {
            function: None,
            line_defined: 12,
            depth: 1,
            hits: vec![
                -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 1, -1, -1
            ],
        }
    );
    assert_eq!(
        report[3],
        CoverageInfo {
            function: None,
            line_defined: 13,
            depth: 2,
            hits: vec![
                -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 1, -1, -1
            ],
        }
    );

    Ok(())
}

#[tokio::test]
async fn test_function_pointer() -> Result<()> {
    let lua = Luau::new();

    let func1 = lua.load("return function() end").into_function()?;
    let func2 = func1.call::<Function>(()).await?;

    assert_eq!(func1.to_pointer(), func1.to_pointer());
    assert_ne!(func1.to_pointer(), func2.to_pointer());

    Ok(())
}
#[tokio::test]
async fn test_function_deep_clone() -> Result<()> {
    let lua = Luau::new();

    lua.globals().set("a", 1)?;
    let func1 = lua.load("a += 1; return a").into_function()?;
    let func2 = func1.deep_clone()?;

    assert_ne!(func1.to_pointer(), func2.to_pointer());
    assert_eq!(func1.call::<i32>(()).await?, 2);
    assert_eq!(func2.call::<i32>(()).await?, 3);

    // Check that for Rust functions deep_clone is just a clone
    let rust_func = lua.create_function(|_, ()| Ok(42))?;
    let rust_func2 = rust_func.deep_clone()?;
    assert_eq!(rust_func.to_pointer(), rust_func2.to_pointer());

    Ok(())
}

