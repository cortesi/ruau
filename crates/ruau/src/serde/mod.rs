//! (De)Serialization support using serde.

use std::os::raw::c_void;

use serde::{de::DeserializeOwned, ser::Serialize};

use crate::{error::Result, state::Luau, table::Table, util::check_stack, value::Value};

impl Luau {
    /// A special value (lightuserdata) used to encode/decode optional (none) values.
    ///
    /// # Example
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use ruau::{Luau, Result};
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let lua = Luau::new();
    ///     lua.globals().set("null", lua.null())?;
    ///
    ///     let val = lua.load(r#"{a = null}"#).eval().await?;
    ///     let map: HashMap<String, Option<String>> = lua.from_value(val)?;
    ///     assert_eq!(map["a"], None);
    ///
    ///     Ok(())
    /// }
    /// ```
    #[must_use]
    pub fn null(&self) -> Value {
        Value::NULL
    }

    /// A metatable attachable to a Luau table to systematically encode it as Array (instead of
    /// Map). As a result, encoded Array will contain only sequence part of the table, with the
    /// same length as the `#` operator on that table.
    ///
    /// # Example
    ///
    /// ```
    /// use ruau::{Luau, Result};
    /// use serde_json::Value as JsonValue;
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let lua = Luau::new();
    ///     lua.globals().set("array_mt", lua.array_metatable())?;
    ///
    ///     // Encode as an empty array (no sequence part in the Luau table)
    ///     let val = lua.load("setmetatable({a = 5}, array_mt)").eval().await?;
    ///     let j: JsonValue = lua.from_value(val)?;
    ///     assert_eq!(j.to_string(), "[]");
    ///
    ///     // Encode as object
    ///     let val = lua.load("{a = 5}").eval().await?;
    ///     let j: JsonValue = lua.from_value(val)?;
    ///     assert_eq!(j.to_string(), r#"{"a":5}"#);
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn array_metatable(&self) -> Table {
        let lua = self.raw();
        unsafe {
            push_array_metatable(lua.ref_thread());
            Table(lua.pop_ref_thread())
        }
    }

    /// Converts `T` into a [`Value`] instance.
    ///
    /// # Example
    ///
    /// ```
    /// use ruau::{Luau, Result};
    /// use serde::Serialize;
    ///
    /// #[derive(Serialize)]
    /// struct User {
    ///     name: String,
    ///     age: u8,
    /// }
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let lua = Luau::new();
    ///     let u = User {
    ///         name: "John Smith".into(),
    ///         age: 20,
    ///     };
    ///     lua.globals().set("user", lua.to_value(&u)?)?;
    ///     lua.load(r#"
    ///         assert(user["name"] == "John Smith")
    ///         assert(user["age"] == 20)
    ///     "#).exec().await
    /// }
    /// ```
    pub fn to_value<T>(&self, t: &T) -> Result<Value>
    where
        T: Serialize + ?Sized,
    {
        t.serialize(ser::Serializer::new(self))
    }

    /// Converts `T` into a [`Value`] instance with the given serialization options.
    ///
    /// # Example
    ///
    /// ```
    /// use ruau::{serde::SerializeOptions, Luau, Result};
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let lua = Luau::new();
    ///     let v = vec![1, 2, 3];
    ///     let options = SerializeOptions::new().set_array_metatable(false);
    ///     lua.globals().set("v", lua.to_value_with(&v, options)?)?;
    ///
    ///     lua.load(r#"
    ///         assert(#v == 3 and v[1] == 1 and v[2] == 2 and v[3] == 3)
    ///         assert(getmetatable(v) == nil)
    ///     "#).exec().await
    /// }
    /// ```
    pub fn to_value_with<T>(&self, t: &T, options: ser::SerializeOptions) -> Result<Value>
    where
        T: Serialize + ?Sized,
    {
        t.serialize(ser::Serializer::new_with_options(self, options))
    }

    /// Deserializes a [`Value`] into any serde deserializable object.
    ///
    /// # Example
    ///
    /// ```
    /// use ruau::{Luau, Result};
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize, Debug, PartialEq)]
    /// struct User {
    ///     name: String,
    ///     age: u8,
    /// }
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let lua = Luau::new();
    ///     let val = lua.load(r#"{name = "John Smith", age = 20}"#).eval().await?;
    ///     let u: User = lua.from_value(val)?;
    ///
    ///     assert_eq!(u, User { name: "John Smith".into(), age: 20 });
    ///
    ///     Ok(())
    /// }
    /// ```
    #[allow(clippy::wrong_self_convention)]
    pub fn from_value<T>(&self, value: Value) -> Result<T>
    where
        T: DeserializeOwned,
    {
        T::deserialize(de::Deserializer::new(value))
    }

    /// Deserializes a [`Value`] into any serde deserializable object with the given options.
    ///
    /// # Example
    ///
    /// ```
    /// use ruau::{serde::DeserializeOptions, Luau, Result};
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize, Debug, PartialEq)]
    /// struct User {
    ///     name: String,
    ///     age: u8,
    /// }
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let lua = Luau::new();
    ///     let val = lua.load(r#"{name = "John Smith", age = 20, f = function() end}"#).eval().await?;
    ///     let options = DeserializeOptions::new().deny_unsupported_types(false);
    ///     let u: User = lua.from_value_with(val, options)?;
    ///
    ///     assert_eq!(u, User { name: "John Smith".into(), age: 20 });
    ///
    ///     Ok(())
    /// }
    /// ```
    #[allow(clippy::wrong_self_convention)]
    pub fn from_value_with<T>(&self, value: Value, options: de::DeserializeOptions) -> Result<T>
    where
        T: DeserializeOwned,
    {
        T::deserialize(de::Deserializer::new_with_options(value, options))
    }
}

// Uses 2 stack spaces and calls checkstack.
pub(crate) unsafe fn init_metatables(state: *mut ffi::lua_State) -> Result<()> {
    check_stack(state, 2)?;
    protect_lua!(state, 0, 0, fn(state) {
        ffi::lua_createtable(state, 0, 1);

        ffi::lua_pushstring(state, cstr!("__metatable"));
        ffi::lua_pushboolean(state, 0);
        ffi::lua_rawset(state, -3);

        let array_metatable_key = &ARRAY_METATABLE_REGISTRY_KEY as *const u8 as *const c_void;
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, array_metatable_key);
    })
}

pub(crate) unsafe fn push_array_metatable(state: *mut ffi::lua_State) {
    let array_metatable_key = &ARRAY_METATABLE_REGISTRY_KEY as *const u8 as *const c_void;
    ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, array_metatable_key);
}

static ARRAY_METATABLE_REGISTRY_KEY: u8 = 0;

pub(crate) mod de;
pub(crate) mod ser;

pub use de::DeserializeOptions;
pub use ser::SerializeOptions;
