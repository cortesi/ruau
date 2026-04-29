use crate::{
    Function,
    error::{Error, Result},
    function::AsyncCallFuture,
    state::WeakLua,
    table::Table,
    traits::{FromLua, FromLuaMulti, IntoLua, IntoLuaMulti, ObjectLike},
    userdata::AnyUserData,
    value::Value,
};

impl ObjectLike for AnyUserData {
    #[inline]
    fn get<V: FromLua>(&self, key: impl IntoLua) -> Result<V> {
        // `lua_gettable` method used under the hood can work with any Lua value
        // that has `__index` metamethod
        Table(self.0.clone()).get_protected(key)
    }

    #[inline]
    fn set(&self, key: impl IntoLua, value: impl IntoLua) -> Result<()> {
        // `lua_settable` method used under the hood can work with any Lua value
        // that has `__newindex` metamethod
        Table(self.0.clone()).set_protected(key, value)
    }

    #[inline]
    fn call<R>(&self, args: impl IntoLuaMulti) -> AsyncCallFuture<R>
    where
        R: FromLuaMulti,
    {
        Function(self.0.clone()).call(args)
    }
    #[inline]
    fn call_sync<R>(&self, args: impl IntoLuaMulti) -> Result<R>
    where
        R: FromLuaMulti,
    {
        Function(self.0.clone()).call_sync(args)
    }

    #[inline]
    fn call_method<R>(&self, name: &str, args: impl IntoLuaMulti) -> AsyncCallFuture<R>
    where
        R: FromLuaMulti,
    {
        self.call_function(name, (self, args))
    }
    fn call_method_sync<R>(&self, name: &str, args: impl IntoLuaMulti) -> Result<R>
    where
        R: FromLuaMulti,
    {
        self.call_function_sync(name, (self, args))
    }

    fn call_function<R>(&self, name: &str, args: impl IntoLuaMulti) -> AsyncCallFuture<R>
    where
        R: FromLuaMulti,
    {
        match self.get(name) {
            Ok(Value::Function(func)) => func.call(args),
            Ok(val) => {
                let msg = format!(
                    "attempt to call a {} value (function '{name}')",
                    val.type_name()
                );
                AsyncCallFuture::error(Error::RuntimeError(msg))
            }
            Err(err) => AsyncCallFuture::error(err),
        }
    }
    fn call_function_sync<R>(&self, name: &str, args: impl IntoLuaMulti) -> Result<R>
    where
        R: FromLuaMulti,
    {
        match self.get(name)? {
            Value::Function(func) => func.call_sync(args),
            val => {
                let msg = format!(
                    "attempt to call a {} value (function '{name}')",
                    val.type_name()
                );
                Err(Error::RuntimeError(msg))
            }
        }
    }

    #[inline]
    fn to_string(&self) -> Result<String> {
        Value::UserData(self.clone()).to_string()
    }

    #[inline]
    fn to_value(&self) -> Value {
        Value::UserData(self.clone())
    }

    #[inline]
    fn weak_lua(&self) -> &WeakLua {
        &self.0.lua
    }
}
