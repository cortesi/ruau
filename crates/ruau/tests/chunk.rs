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

#[cfg(not(target_os = "wasi"))]
use std::{fs, io};

use ruau::{Chunk, Luau, Result};

#[tokio::test]
async fn test_chunk_methods() -> Result<()> {
    let lua = Luau::new();

    let env = lua.create_table_from([("a", 987)])?;
    let chunk = lua.load("return a").name("@example").environment(env.clone());
    assert_eq!(chunk.call::<i32>(()).await?, 987);

    Ok(())
}

#[tokio::test]
#[cfg(not(target_os = "wasi"))]
async fn test_chunk_path() -> Result<()> {
    let lua = Luau::new();

    if cfg!(target_arch = "wasm32") {
        // TODO: figure out why emscripten fails on file operations
        // Also see https://github.com/rust-lang/rust/issues/119250
        return Ok(());
    }

    let temp_dir = tempfile::tempdir().unwrap();
    fs::write(
        temp_dir.path().join("module.lua"),
        r#"
        return 321
    "#,
    )?;
    let i: i32 = lua.load(temp_dir.path().join("module.lua")).eval().await?;
    assert_eq!(i, 321);

    match lua.load(&*temp_dir.path().join("module2.lua")).exec().await {
        Err(err) if err.downcast_ref::<io::Error>().unwrap().kind() == io::ErrorKind::NotFound => {}
        res => panic!("expected io::Error, got {:?}", res),
    };

    // &Path
    assert_eq!(
        (lua.load(temp_dir.path().join("module.lua").as_path()))
            .eval::<i32>()
            .await?,
        321
    );

    Ok(())
}

#[tokio::test]
async fn test_chunk_impls() -> Result<()> {
    let lua = Luau::new();

    // StdString
    assert_eq!(lua.load(String::from("1")).eval::<i32>().await?, 1);
    assert_eq!(lua.load(String::from("2")).eval::<i32>().await?, 2);

    // &[u8]
    assert_eq!(lua.load(&b"3"[..]).eval::<i32>().await?, 3);

    // Vec<u8>
    assert_eq!(lua.load(b"4".to_vec()).eval::<i32>().await?, 4);
    assert_eq!(lua.load(b"5".to_vec()).eval::<i32>().await?, 5);

    Ok(())
}

#[tokio::test]
#[cfg(feature = "macros")]
async fn test_chunk_macro() -> Result<()> {
    let lua = Luau::new();

    let name = "Rustacean";
    let table = vec![1];

    let data = lua.create_table()?;
    data.raw_set("num", 1)?;

    let ud = ruau::AnyUserData::wrap("hello");
    let f = lua.create_function(|_, ()| Ok(()))?;

    lua.globals().set("g", 123)?;

    let string = String::new();
    let str = string.as_str();

    lua.load(ruau::chunk! {
        assert($name == "Rustacean")
        assert(type($table) == "table")
        assert($table[1] == 1)
        assert(type($data) == "table")
        assert($data.num == 1)
        assert(type($ud) == "userdata")
        assert(type($f) == "function")
        assert(type($str) == "string")
        assert($str == "")
        assert(g == 123)
        s = 321
    })
    .exec()
    .await?;

    assert_eq!(lua.globals().get::<i32>("s")?, 321);

    Ok(())
}
#[tokio::test]
async fn test_compiler() -> Result<()> {
    let compiler = ruau::Compiler::new()
        .optimization_level(ruau::compiler::OptimizationLevel::Release)
        .debug_level(ruau::compiler::DebugLevel::Full)
        .type_info_level(ruau::compiler::TypeInfoLevel::AllModules)
        .coverage_level(ruau::compiler::CoverageLevel::StatementAndExpression)
        .mutable_globals(["mutable_global"])
        .userdata_types(["MyUserdata"])
        .disabled_builtins(["tostring"]);

    assert!(
        compiler
            .compile("return tostring(vector.create(1, 2, 3))")
            .is_ok()
    );

    // Error
    match compiler.compile("%") {
        Err(ruau::Error::SyntaxError { ref message, .. }) => {
            assert!(message.contains("Expected identifier when parsing expression, got '%'"),);
        }
        res => panic!("expected result: {res:?}"),
    }

    Ok(())
}
#[tokio::test]
async fn test_compiler_library_constants() {
    use ruau::{Compiler, Vector};

    let compiler = Compiler::new()
        .optimization_level(ruau::compiler::OptimizationLevel::Release)
        .add_library_constant("mylib.const_bool", true)
        .add_library_constant("mylib.const_num", 123.0)
        .add_library_constant("mylib.const_vec", Vector::zero())
        .add_library_constant("mylib.const_str", "value1")
        .add_vector_constant("one", [1.0, 1.0, 1.0]);

    let lua = Luau::new();
    lua.set_compiler(compiler);
    let const_bool = lua.load("return mylib.const_bool").eval::<bool>().await.unwrap();
    assert!(const_bool);
    let const_num = lua.load("return mylib.const_num").eval::<f64>().await.unwrap();
    assert_eq!(const_num, 123.0);
    let const_vec = lua.load("return mylib.const_vec").eval::<Vector>().await.unwrap();
    assert_eq!(const_vec, Vector::zero());
    let vector_one = lua.load("return vector.one").eval::<Vector>().await.unwrap();
    assert_eq!(vector_one, Vector::new(1.0, 1.0, 1.0));
    let const_str = lua.load("return mylib.const_str").eval::<String>().await;
    assert_eq!(const_str.unwrap(), "value1");
}

#[tokio::test]
async fn test_chunk_wrap() -> Result<()> {
    let lua = Luau::new();

    let f = Chunk::wrap("return 123");
    lua.globals().set("f", f)?;
    lua.load("assert(f() == 123)").exec().await.unwrap();

    lua.globals().set("f2", Chunk::wrap("c()"))?;
    assert!(
        (lua.load("f2()").exec().await.err().unwrap().to_string()).contains(file!()),
        "wrong chunk location"
    );

    Ok(())
}
