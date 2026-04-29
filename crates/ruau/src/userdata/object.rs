use crate::{
    Function,
    error::{Error, Result},
    state::WeakLuau,
    table::Table,
    traits::{FromLuau, FromLuauMulti, IntoLuau, IntoLuauMulti, ObjectLike},
    userdata::AnyUserData,
    value::Value,
};

impl ObjectLike for AnyUserData {
    #[inline]
    fn get<V: FromLuau>(&self, key: impl IntoLuau) -> Result<V> {
        // `lua_gettable` method used under the hood can work with any Luau value
        // that has `__index` metamethod
        Table(self.0.clone()).get_protected(key)
    }

    #[inline]
    fn set(&self, key: impl IntoLuau, value: impl IntoLuau) -> Result<()> {
        // `lua_settable` method used under the hood can work with any Luau value
        // that has `__newindex` metamethod
        Table(self.0.clone()).set_protected(key, value)
    }

    #[inline]
    async fn call<R>(&self, args: impl IntoLuauMulti) -> Result<R>
    where
        R: FromLuauMulti,
    {
        Function(self.0.clone()).call(args).await
    }

    #[inline]
    async fn call_method<R>(&self, name: &str, args: impl IntoLuauMulti) -> Result<R>
    where
        R: FromLuauMulti,
    {
        self.call_function(name, (self, args)).await
    }

    async fn call_function<R>(&self, name: &str, args: impl IntoLuauMulti) -> Result<R>
    where
        R: FromLuauMulti,
    {
        match self.get(name) {
            Ok(Value::Function(func)) => func.call(args).await,
            Ok(val) => {
                let msg = format!(
                    "attempt to call a {} value (function '{name}')",
                    val.type_name()
                );
                Err(Error::RuntimeError(msg))
            }
            Err(err) => Err(err),
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
    fn weak_lua(&self) -> &WeakLuau {
        &self.0.lua
    }
}
