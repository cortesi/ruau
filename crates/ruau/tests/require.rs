//! require integration tests.

use std::{
    env::current_dir, fs::write, future::Future, pin::Pin, result::Result as StdResult,
    time::Duration,
};

use ruau::{
    FromLuau, IntoLuau, Luau, MultiValue, Result, Value,
    resolver::{
        FilesystemResolver, InMemoryResolver, ModuleId, ModuleResolveError, ModuleResolver,
        ModuleSource,
    },
};
use tokio::time::sleep;

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingResolver;

    impl ModuleResolver for FailingResolver {
        fn resolve<'a>(
            &'a self,
            _requester: Option<&'a ModuleId>,
            specifier: &'a str,
        ) -> Pin<Box<dyn Future<Output = StdResult<ModuleSource, ModuleResolveError>> + 'a>>
        {
            let specifier = specifier.to_owned();
            Box::pin(async move {
                Err(ModuleResolveError::Read {
                    module: specifier,
                    message: "test error".to_owned(),
                })
            })
        }
    }

    struct InterfaceResolver;

    impl ModuleResolver for InterfaceResolver {
        fn resolve<'a>(
            &'a self,
            _requester: Option<&'a ModuleId>,
            specifier: &'a str,
        ) -> Pin<Box<dyn Future<Output = StdResult<ModuleSource, ModuleResolveError>> + 'a>>
        {
            Box::pin(async move {
                Ok(ModuleSource::interface(
                    specifier.to_owned(),
                    "export type Module = { value: number }",
                ))
            })
        }
    }

    /// Returns a fresh `Luau` with the filesystem resolver rooted at the current working directory
    /// — the default the tests in this file expect.
    fn lua_with_fs_resolver() -> Luau {
        let lua = Luau::new();
        let cwd = current_dir().expect("cwd");
        lua.set_module_resolver(FilesystemResolver::new(cwd))
            .expect("install resolver");
        lua
    }

    fn lua_with_fs_extensions(extensions: impl IntoIterator<Item = &'static str>) -> Luau {
        let lua = Luau::new();
        let cwd = current_dir().expect("cwd");
        lua.set_module_resolver(FilesystemResolver::new(cwd).with_extensions(extensions))
            .expect("install resolver");
        lua
    }

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

    async fn assert_require_error_contains(lua: &Luau, path: impl IntoLuau, expected: &str) {
        let err = run_require(lua, path)
            .await
            .expect_err("require should fail");
        assert!(
            err.to_string().contains(expected),
            "expected error containing {expected:?}, got {err}"
        );
    }

    async fn assert_require_not_found(lua: &Luau, path: impl IntoLuau) {
        assert_require_error_contains(lua, path, "module not found").await;
    }

    async fn assert_required_fields(lua: &Luau, path: &str, expected_fields: &[(i64, &str)]) {
        let value = run_require(lua, path).await.expect("require");
        for &(key, expected) in expected_fields {
            assert_eq!(expected, get_str(&value, key), "{path}[{key}]");
        }
    }

    #[tokio::test]
    async fn test_require_errors() {
        let lua = lua_with_fs_resolver();

        for path in [
            "/an/absolute/path",
            "an/unprefixed/path",
            "@this.alias.does.not.exist",
            "@",
        ] {
            assert_require_not_found(&lua, path).await;
        }

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

        lua.set_module_resolver(FailingResolver).unwrap();
        let res = lua
            .load(r#"return require('./a/relative/path')"#)
            .exec()
            .await;
        assert!((res.unwrap_err().to_string()).contains("test error"));
    }

    #[tokio::test]
    async fn require_rejects_interface_modules() {
        let lua = Luau::new();
        lua.set_module_resolver(InterfaceResolver)
            .expect("install resolver");

        let err = run_require(&lua, "iface")
            .await
            .expect_err("interface module");
        assert!(err.to_string().contains("module is not executable: iface"));
        assert!(err.to_string().contains("ModuleInterfaceSet"));
    }

    #[tokio::test]
    async fn require_reports_module_cycles() {
        let lua = Luau::new();
        let resolver = InMemoryResolver::new()
            .with_module("a", "return require('b')")
            .with_module("b", "return require('a')");
        lua.set_module_resolver(resolver).expect("install resolver");

        let err = run_require(&lua, "a").await.expect_err("cyclic require");
        assert!(err.to_string().contains("cyclic module require: a"));
    }

    #[tokio::test]
    async fn test_require_without_config() {
        let lua = lua_with_fs_resolver();

        for (path, expected_fields) in [
            (
                "./tests/luau/require/without_config/dependency",
                &[(1, "result from dependency")][..],
            ),
            (
                "./tests/luau/require/without_config/module",
                &[(1, "result from dependency"), (2, "required into module")][..],
            ),
            (
                "./tests/luau/require/without_config/luau",
                &[(1, "result from init.luau")][..],
            ),
            (
                "./tests/luau/require/without_config/nested_module_requirer",
                &[(1, "result from submodule")][..],
            ),
            (
                "./tests/luau/require/without_config/nested",
                &[(1, "result from submodule")][..],
            ),
            (
                "./tests/luau/require/without_config/nested_inits_requirer",
                &[
                    (1, "result from nested_inits/init"),
                    (2, "required into module"),
                ][..],
            ),
            (
                "./tests/luau/require/without_config/ambiguous_file_requirer",
                &[(1, "result from dependency"), (2, "required into module")][..],
            ),
            (
                "./tests/luau/require/without_config/ambiguous_directory_requirer",
                &[(1, "result from dependency"), (2, "required into module")][..],
            ),
        ] {
            assert_required_fields(&lua, path, expected_fields).await;
        }

        // RequireSimpleRelativePathWithinPcall
        let res = run_require_pcall(&lua, "./tests/luau/require/without_config/dependency")
            .await
            .unwrap();
        assert!(res[0].as_boolean().unwrap());
        assert_eq!("result from dependency", get_str(&res[1], 1));

        // RequireLua requires an explicit extension override.
        assert_require_not_found(&lua, "./tests/luau/require/without_config/lua_dependency").await;

        let lua_with_lua = lua_with_fs_extensions(["luau", "lua"]);
        assert_required_fields(
            &lua_with_lua,
            "./tests/luau/require/without_config/lua_dependency",
            &[(1, "result from lua_dependency")],
        )
        .await;

        // RequireInitLua requires an explicit extension override.
        assert_require_not_found(&lua, "./tests/luau/require/without_config/lua").await;
        assert_required_fields(
            &lua_with_lua,
            "./tests/luau/require/without_config/lua",
            &[(1, "result from init.lua")],
        )
        .await;

        // CannotRequireInitLuauDirectly
        assert_require_not_found(&lua, "./tests/luau/require/without_config/nested/init").await;

        // CheckCachedResult
        let res = run_require(&lua, "./tests/luau/require/without_config/validate_cache")
            .await
            .unwrap();
        assert!(res.is_table());
    }

    async fn assert_config_aliases_are_app_policy(config_type: &str) {
        let lua = lua_with_fs_resolver();

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
        let lua = lua_with_fs_resolver();

        let cwd = current_dir().unwrap();
        let temp_dir = tempfile::Builder::new()
            .prefix(".ruau-require-")
            .tempdir_in(&cwd)
            .unwrap();
        let temp_path = temp_dir.path().join("async_chunk.luau");
        write(
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
                sleep(Duration::from_millis(ms)).await;
                Ok(())
            })?,
        )?;
        lua.globals()
            .set("tmp_module", temp_path.to_str().unwrap())?;

        lua.load(
            r#"
        local result = require(tmp_module)
        assert(result == "result_after_async_sleep")
        "#,
        )
        .exec()
        .await
    }
}
