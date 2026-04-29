use ruau::{
    FromLuau, IntoLuau, Luau, MultiValue, Result, Value,
    resolver::{ModuleResolveError, ModuleResolver, ModuleSource},
};

async fn run_require(lua: &Luau, path: impl IntoLuau) -> Result<Value> {
    lua.load(r#"return require(...)"#).call(path).await
}

async fn run_require_pcall(lua: &Luau, path: impl IntoLuau) -> Result<MultiValue> {
    lua.load(r#"return pcall(require, ...)"#).call(path).await
}

#[track_caller]
fn get_value<V: FromLuau>(value: &Value, key: impl IntoLuau) -> V {
    value.as_table().unwrap().get(key).unwrap()
}

#[track_caller]
fn get_str(value: &Value, key: impl IntoLuau) -> String {
    get_value(value, key)
}

#[tokio::test]
async fn test_require_errors() {
    let lua = Luau::new();

    // RequireAbsolutePath
    let res = run_require(&lua, "/an/absolute/path").await;
    assert!(res.is_err());
    assert!((res.unwrap_err().to_string()).contains("module not found"));

    // RequireUnprefixedMissingPath
    let res = run_require(&lua, "an/unprefixed/path").await;
    assert!(res.is_err());
    assert!((res.unwrap_err().to_string()).contains("module not found"));

    // Pass non-string to require
    let res = run_require(&lua, true).await;
    assert!(res.is_err());

    // Require from loadstring
    let res = lua
        .load(r#"return loadstring("require('./a/relative/path')")()"#)
        .eval::<Value>()
        .await;
    assert!(res.is_err());
    assert!((res.unwrap_err().to_string()).contains("module not found"));

    // RequireAliasThatDoesNotExist
    let res = run_require(&lua, "@this.alias.does.not.exist").await;
    assert!(res.is_err());
    assert!((res.unwrap_err().to_string()).contains("module not found"));

    // IllegalAlias
    let res = run_require(&lua, "@").await;
    assert!(res.is_err());
    assert!((res.unwrap_err().to_string()).contains("module not found"));

    struct FailingResolver;

    #[async_trait::async_trait(?Send)]
    impl ModuleResolver for FailingResolver {
        async fn resolve(
            &self,
            _requester: Option<&ruau::resolver::ModuleId>,
            specifier: &str,
        ) -> std::result::Result<ModuleSource, ModuleResolveError> {
            Err(ModuleResolveError::Read {
                module: specifier.to_owned(),
                message: "test error".to_owned(),
            })
        }
    }

    lua.set_module_resolver(FailingResolver).unwrap();
    let res = lua
        .load(r#"return require('./a/relative/path')"#)
        .exec()
        .await;
    assert!((res.unwrap_err().to_string()).contains("test error"));
}

#[tokio::test]
async fn test_require_without_config() {
    let lua = Luau::new();

    // RequireSimpleRelativePath
    let res = run_require(&lua, "./tests/luau/require/without_config/dependency")
        .await
        .unwrap();
    assert_eq!("result from dependency", get_str(&res, 1));

    // RequireSimpleRelativePathWithinPcall
    let res = run_require_pcall(&lua, "./tests/luau/require/without_config/dependency")
        .await
        .unwrap();
    assert!(res[0].as_boolean().unwrap());
    assert_eq!("result from dependency", get_str(&res[1], 1));

    // RequireRelativeToRequiringFile
    let res = run_require(&lua, "./tests/luau/require/without_config/module")
        .await
        .unwrap();
    assert_eq!("result from dependency", get_str(&res, 1));
    assert_eq!("required into module", get_str(&res, 2));

    // RequireLua
    let res = run_require(&lua, "./tests/luau/require/without_config/lua_dependency")
        .await
        .unwrap();
    assert_eq!("result from lua_dependency", get_str(&res, 1));

    // RequireInitLuau
    let res = run_require(&lua, "./tests/luau/require/without_config/luau")
        .await
        .unwrap();
    assert_eq!("result from init.luau", get_str(&res, 1));

    // RequireInitLua
    let res = run_require(&lua, "./tests/luau/require/without_config/lua")
        .await
        .unwrap();
    assert_eq!("result from init.lua", get_str(&res, 1));

    // RequireSubmoduleUsingSelfIndirectly
    let res = run_require(
        &lua,
        "./tests/luau/require/without_config/nested_module_requirer",
    )
    .await
    .unwrap();
    assert_eq!("result from submodule", get_str(&res, 1));

    // RequireSubmoduleUsingSelfDirectly
    let res = run_require(&lua, "./tests/luau/require/without_config/nested")
        .await
        .unwrap();
    assert_eq!("result from submodule", get_str(&res, 1));

    // CannotRequireInitLuauDirectly
    let res = run_require(&lua, "./tests/luau/require/without_config/nested/init").await;
    assert!(res.is_err());
    assert!((res.unwrap_err().to_string()).contains("module not found"));

    // RequireNestedInits
    let res = run_require(
        &lua,
        "./tests/luau/require/without_config/nested_inits_requirer",
    )
    .await
    .unwrap();
    assert_eq!("result from nested_inits/init", get_str(&res, 1));
    assert_eq!("required into module", get_str(&res, 2));

    // RequireWithFileAmbiguity
    let res = run_require(
        &lua,
        "./tests/luau/require/without_config/ambiguous_file_requirer",
    )
    .await;
    assert!(res.is_err());
    assert!((res.unwrap_err().to_string()).contains("module is ambiguous"));

    // RequireWithDirectoryAmbiguity
    let res = run_require(
        &lua,
        "./tests/luau/require/without_config/ambiguous_directory_requirer",
    )
    .await;
    assert!(res.is_err());
    assert!((res.unwrap_err().to_string()).contains("module is ambiguous"));

    // CheckCachedResult
    let res = run_require(&lua, "./tests/luau/require/without_config/validate_cache")
        .await
        .unwrap();
    assert!(res.is_table());
}

async fn assert_config_aliases_are_app_policy(config_type: &str) {
    let lua = Luau::new();

    let base_path = format!("./tests/luau/require/{config_type}");

    let res = run_require(&lua, format!("{base_path}/src/alias_requirer")).await;
    assert!(res.is_err());
    assert!((res.unwrap_err().to_string()).contains("module not found"));
}

#[tokio::test]
async fn test_require_does_not_read_luaurc_aliases() {
    assert_config_aliases_are_app_policy("with_config").await;
}

#[tokio::test]
async fn test_require_does_not_read_config_luau_aliases() {
    assert_config_aliases_are_app_policy("with_config_luau").await;
}

#[cfg(not(windows))]
#[tokio::test]
async fn test_async_require() -> Result<()> {
    let lua = Luau::new();

    let temp_dir = tempfile::tempdir().unwrap();
    let temp_path = temp_dir.path().join("async_chunk.luau");
    std::fs::write(
        &temp_path,
        r#"
        sleep_ms(10)
        return "result_after_async_sleep"
    "#,
    )
    .unwrap();

    lua.globals().set(
        "sleep_ms",
        lua.create_async_function(async |_, ms: u64| {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            Ok(())
        })?,
    )?;
    lua.globals()
        .set("tmp_dir", temp_dir.path().to_str().unwrap())?;
    lua.globals().set(
        "curr_dir_components",
        std::env::current_dir().unwrap().components().count(),
    )?;

    lua.load(
        r#"
        local path_to_root = string.rep("/..", curr_dir_components - 1)
        local result = require(`.{path_to_root}{tmp_dir}/async_chunk`)
        assert(result == "result_after_async_sleep")
        "#,
    )
    .exec()
    .await
}
