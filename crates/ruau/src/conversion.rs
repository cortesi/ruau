use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::{CStr, CString, OsStr, OsString},
    hash::{BuildHasher, Hash},
    os::raw::c_int,
    path::{Path, PathBuf},
    slice, str,
};

use bstr::{BStr, BString, ByteVec};
use either::Either;
use num_traits::cast;

use crate::{
    error::{Error, Result},
    function::Function,
    state::{Luau, RawLuau},
    string::{BorrowedBytes, BorrowedStr, LuauString},
    table::Table,
    thread::Thread,
    traits::{FromLuau, IntoLuau, ShortTypeName as _, StackCtx},
    types::{LightUserData, RegistryKey},
    userdata_impl::{AnyUserData, UserData},
    value::{Nil, Value},
};

impl IntoLuau for Value {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(self)
    }
}

impl IntoLuau for &Value {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(self.clone())
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        // SAFETY: ctx proves the caller reserved stack space.
        unsafe { lua.push_value(self) }
    }
}

impl FromLuau for Value {
    #[inline]
    fn from_luau(lua_value: Value, _: &Luau) -> Result<Self> {
        Ok(lua_value)
    }
}

impl IntoLuau for LuauString {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::String(self))
    }
}

impl IntoLuau for &LuauString {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::String(self.clone()))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.0);
        Ok(())
    }
}

impl FromLuau for LuauString {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        let ty = value.type_name();
        value.coerce_string(lua)?.ok_or_else(|| {
            Error::from_luau_conversion(ty, "string", "expected string or number".to_string())
        })
    }

    fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let lua = ctx.lua;
        let state = lua.state();
        // SAFETY: ctx proves `idx` is a valid stack index.
        unsafe {
            let type_id = ffi::lua_type(state, idx);
            if type_id == ffi::LUA_TSTRING {
                ffi::lua_xpush(state, lua.ref_thread(), idx);
                return Ok(Self(lua.pop_ref_thread()));
            }
            // Fallback to default
            Self::from_luau(lua.stack_value(idx, Some(type_id)), lua.lua())
        }
    }
}

impl IntoLuau for BorrowedStr {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::String(LuauString(self.vref)))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.vref);
        Ok(())
    }
}

impl IntoLuau for &BorrowedStr {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::String(LuauString(self.vref.clone())))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.vref);
        Ok(())
    }
}

impl FromLuau for BorrowedStr {
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        let s = LuauString::from_luau(value, lua)?;
        Self::try_from(&s)
    }

    fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let s = LuauString::from_stack(idx, ctx)?;
        Self::try_from(&s)
    }
}

impl IntoLuau for BorrowedBytes {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::String(LuauString(self.vref)))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.vref);
        Ok(())
    }
}

impl IntoLuau for &BorrowedBytes {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::String(LuauString(self.vref.clone())))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.vref);
        Ok(())
    }
}

impl FromLuau for BorrowedBytes {
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        let s = LuauString::from_luau(value, lua)?;
        Ok(Self::from(&s))
    }

    fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let s = LuauString::from_stack(idx, ctx)?;
        Ok(Self::from(&s))
    }
}

impl IntoLuau for Table {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Table(self))
    }
}

impl IntoLuau for &Table {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Table(self.clone()))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.0);
        Ok(())
    }
}

impl FromLuau for Table {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::Table(table) => Ok(table),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                "table",
                None,
            )),
        }
    }
}

impl IntoLuau for Function {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Function(self))
    }
}

impl IntoLuau for &Function {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Function(self.clone()))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.0);
        Ok(())
    }
}

impl FromLuau for Function {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::Function(table) => Ok(table),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                "function",
                None,
            )),
        }
    }
}

impl IntoLuau for Thread {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Thread(self))
    }
}

impl IntoLuau for &Thread {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Thread(self.clone()))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.0);
        Ok(())
    }
}

impl FromLuau for Thread {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::Thread(t) => Ok(t),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                "thread",
                None,
            )),
        }
    }
}

impl IntoLuau for AnyUserData {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::UserData(self))
    }
}

impl IntoLuau for &AnyUserData {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::UserData(self.clone()))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.0);
        Ok(())
    }
}

impl FromLuau for AnyUserData {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::UserData(ud) => Ok(ud),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                "userdata",
                None,
            )),
        }
    }
}

impl<T: UserData + 'static> IntoLuau for T {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::UserData(lua.create_userdata(self)?))
    }
}

impl IntoLuau for Error {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Error(Box::new(self)))
    }
}

impl FromLuau for Error {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Error> {
        match value {
            Value::Error(err) => Ok(*err),
            val => Ok(Self::runtime(val.to_string()?)),
        }
    }
}

impl IntoLuau for RegistryKey {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        lua.registry().get(&self)
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        <&Self>::push_into_stack(&self, ctx)
    }
}

impl IntoLuau for &RegistryKey {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        lua.registry().get(self)
    }

    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        if !lua.owns_registry_value(self) {
            return Err(Error::MismatchedRegistryKey);
        }

        // SAFETY: ctx proves stack reservation; lua_pushnil and lua_rawgeti on a registry index
        // cannot raise.
        unsafe {
            match self.id() {
                ffi::LUA_REFNIL => ffi::lua_pushnil(lua.state()),
                id => {
                    ffi::lua_rawgeti(lua.state(), ffi::LUA_REGISTRYINDEX, id as _);
                }
            }
        }
        Ok(())
    }
}

impl FromLuau for RegistryKey {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        lua.registry().insert(value)
    }
}

impl IntoLuau for bool {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Boolean(self))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        // SAFETY: ctx proves stack reservation; lua_pushboolean cannot raise.
        unsafe { ffi::lua_pushboolean(lua.state(), self as c_int) };
        Ok(())
    }
}

impl FromLuau for bool {
    #[inline]
    fn from_luau(v: Value, _: &Luau) -> Result<Self> {
        match v {
            Value::Nil => Ok(false),
            Value::Boolean(b) => Ok(b),
            _ => Ok(true),
        }
    }

    #[inline]
    fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let lua = ctx.lua;
        // SAFETY: ctx proves `idx` is valid; lua_toboolean is a pure read.
        Ok(unsafe { ffi::lua_toboolean(lua.state(), idx) } != 0)
    }
}

impl IntoLuau for LightUserData {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::LightUserData(self))
    }
}

impl FromLuau for LightUserData {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::LightUserData(ud) => Ok(ud),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                "lightuserdata",
                None,
            )),
        }
    }
}
impl IntoLuau for crate::Vector {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Vector(self))
    }
}
impl FromLuau for crate::Vector {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::Vector(v) => Ok(v),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                "vector",
                None,
            )),
        }
    }
}
impl IntoLuau for crate::Buffer {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Buffer(self))
    }
}
impl IntoLuau for &crate::Buffer {
    #[inline]
    fn into_luau(self, _: &Luau) -> Result<Value> {
        Ok(Value::Buffer(self.clone()))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_ref(&self.0);
        Ok(())
    }
}
impl FromLuau for crate::Buffer {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::Buffer(buf) => Ok(buf),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                "buffer",
                None,
            )),
        }
    }
}

impl IntoLuau for String {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::String(lua.create_string(self)?))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        push_bytes_into_stack(self, lua)
    }
}

impl FromLuau for String {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        let ty = value.type_name();
        Ok(value
            .coerce_string(lua)?
            .ok_or_else(|| {
                Error::from_luau_conversion(
                    ty,
                    Self::type_name(),
                    "expected string or number".to_string(),
                )
            })?
            .to_str()?
            .to_owned())
    }

    #[inline]
    fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let lua = ctx.lua;
        let state = lua.state();
        // SAFETY: ctx proves `idx` is valid; lua_type, lua_tolstring read the slot, and
        // slice::from_raw_parts borrows from Luau's stable string storage for the call
        // duration.
        unsafe {
            let type_id = ffi::lua_type(state, idx);
            if type_id == ffi::LUA_TSTRING {
                let mut size = 0;
                let data = ffi::lua_tolstring(state, idx, &mut size);
                let bytes = slice::from_raw_parts(data as *const u8, size);
                return str::from_utf8(bytes).map(|s| s.to_owned()).map_err(|e| {
                    Error::from_luau_conversion("string", Self::type_name(), e.to_string())
                });
            }
            // Fallback to default
            Self::from_luau(lua.stack_value(idx, Some(type_id)), lua.lua())
        }
    }
}

impl IntoLuau for &str {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::String(lua.create_string(self)?))
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        push_bytes_into_stack(self, lua)
    }
}

impl IntoLuau for Cow<'_, str> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        match self {
            Cow::Borrowed(s) => s.into_luau(lua),
            Cow::Owned(s) => s.into_luau(lua),
        }
    }
}

impl IntoLuau for Box<str> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::String(lua.create_string(&*self)?))
    }
}

impl FromLuau for Box<str> {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        let ty = value.type_name();
        Ok(value
            .coerce_string(lua)?
            .ok_or_else(|| {
                Error::from_luau_conversion(
                    ty,
                    Self::type_name(),
                    "expected string or number".to_string(),
                )
            })?
            .to_str()?
            .to_owned()
            .into_boxed_str())
    }
}

impl IntoLuau for CString {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::String(lua.create_string(self.as_bytes())?))
    }
}

impl FromLuau for CString {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        let ty = value.type_name();
        let string = value.coerce_string(lua)?.ok_or_else(|| {
            Error::from_luau_conversion(
                ty,
                Self::type_name(),
                "expected string or number".to_string(),
            )
        })?;
        match CStr::from_bytes_with_nul(&string.as_bytes_with_nul()) {
            Ok(s) => Ok(s.into()),
            Err(err) => Err(Error::from_luau_conversion(
                ty,
                Self::type_name(),
                err.to_string(),
            )),
        }
    }
}

impl IntoLuau for &CStr {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::String(lua.create_string(self.to_bytes())?))
    }
}

impl IntoLuau for Cow<'_, CStr> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        match self {
            Cow::Borrowed(s) => s.into_luau(lua),
            Cow::Owned(s) => s.into_luau(lua),
        }
    }
}

impl IntoLuau for BString {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::String(lua.create_string(self)?))
    }
}

impl FromLuau for BString {
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        let ty = value.type_name();
        match value {
            Value::String(s) => Ok((*s.as_bytes()).into()),
            Value::Buffer(buf) => Ok(buf.to_vec().into()),
            _ => Ok((*value
                .coerce_string(lua)?
                .ok_or_else(|| {
                    Error::from_luau_conversion(
                        ty,
                        Self::type_name(),
                        "expected string or number".to_string(),
                    )
                })?
                .as_bytes())
            .into()),
        }
    }

    fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let lua = ctx.lua;
        let state = lua.state();
        // SAFETY: ctx proves `idx` is valid; lua_type, lua_tolstring, lua_tobuffer all read
        // the slot and return Luau-owned storage we copy out via `.into()`.
        unsafe {
            match ffi::lua_type(state, idx) {
                ffi::LUA_TSTRING => {
                    let mut size = 0;
                    let data = ffi::lua_tolstring(state, idx, &mut size);
                    Ok(slice::from_raw_parts(data as *const u8, size).into())
                }
                ffi::LUA_TBUFFER => {
                    let mut size = 0;
                    let buf = ffi::lua_tobuffer(state, idx, &mut size);
                    ruau_assert!(!buf.is_null(), "invalid Luau buffer");
                    Ok(slice::from_raw_parts(buf as *const u8, size).into())
                }
                type_id => {
                    // Fallback to default
                    Self::from_luau(lua.stack_value(idx, Some(type_id)), lua.lua())
                }
            }
        }
    }
}

impl IntoLuau for &BStr {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::String(lua.create_string(self)?))
    }
}

impl IntoLuau for OsString {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        self.as_os_str().into_luau(lua)
    }
}

impl FromLuau for OsString {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        let ty = value.type_name();
        let bs = BString::from_luau(value, lua)?;
        Vec::from(bs)
            .into_os_string()
            .map_err(|err| Error::from_luau_conversion(ty, "OsString", err.to_string()))
    }
}

impl IntoLuau for &OsStr {
    #[cfg(unix)]
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        use std::os::unix::ffi::OsStrExt;
        Ok(Value::String(lua.create_string(self.as_bytes())?))
    }

    #[cfg(not(unix))]
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        self.display().to_string().into_luau(lua)
    }
}

impl IntoLuau for PathBuf {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        self.as_os_str().into_luau(lua)
    }
}

impl FromLuau for PathBuf {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        OsString::from_luau(value, lua).map(Self::from)
    }
}

impl IntoLuau for &Path {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        self.as_os_str().into_luau(lua)
    }
}

impl IntoLuau for char {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        let mut char_bytes = [0; 4];
        self.encode_utf8(&mut char_bytes);
        Ok(Value::String(
            lua.create_string(&char_bytes[..self.len_utf8()])?,
        ))
    }
}

impl FromLuau for char {
    fn from_luau(value: Value, _lua: &Luau) -> Result<Self> {
        let ty = value.type_name();
        match value {
            Value::Integer(i) => cast(i).and_then(Self::from_u32).ok_or_else(|| {
                let msg = "integer out of range when converting to char";
                Error::from_luau_conversion(ty, "char", msg.to_string())
            }),
            Value::String(s) => {
                let str = s.to_str()?;
                let mut str_iter = str.chars();
                match (str_iter.next(), str_iter.next()) {
                    (Some(char), None) => Ok(char),
                    _ => {
                        let msg =
                            "expected string to have exactly one char when converting to char";
                        Err(Error::from_luau_conversion(ty, "char", msg.to_string()))
                    }
                }
            }
            _ => {
                let msg = "expected string or integer";
                Err(Error::from_luau_conversion(
                    ty,
                    Self::type_name(),
                    msg.to_string(),
                ))
            }
        }
    }
}

#[inline]
fn push_bytes_into_stack<T>(this: T, lua: &RawLuau) -> Result<()>
where
    T: IntoLuau + AsRef<[u8]>,
{
    let bytes = this.as_ref();
    // SAFETY: callers hold a `&StackCtx` that proves stack space is reserved; lua_pushlstring
    // is sound for the fast path and push_value is the protected fallback.
    unsafe {
        if lua.unlikely_memory_error() && bytes.len() < (1 << 30) {
            // Fast path: push directly into the Luau stack.
            ffi::lua_pushlstring(lua.state(), bytes.as_ptr() as *const _, bytes.len());
            return Ok(());
        }
        // Fallback to default
        lua.push_value(&T::into_luau(this, lua.lua())?)
    }
}

macro_rules! lua_convert_int {
    ($x:ty) => {
        impl IntoLuau for $x {
            #[inline]
            fn into_luau(self, _: &Luau) -> Result<Value> {
                Ok(cast(self)
                    .map(Value::Integer)
                    .unwrap_or_else(|| Value::Number(self as ffi::lua_Number)))
            }

            #[inline]
            fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
                let lua = ctx.lua;
                // SAFETY: ctx proves stack reservation; pushinteger/pushnumber cannot raise.
                unsafe {
                    match cast(self) {
                        Some(i) => ffi::lua_pushinteger(lua.state(), i),
                        None => ffi::lua_pushnumber(lua.state(), self as ffi::lua_Number),
                    }
                }
                Ok(())
            }
        }

        impl FromLuau for $x {
            #[inline]
            fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
                let ty = value.type_name();
                (match value {
                    Value::Integer(i) => cast(i),
                    Value::Number(n) => cast(n),
                    _ => {
                        if let Some(i) = value.coerce_integer(lua)? {
                            cast(i)
                        } else {
                            cast(value.coerce_number(lua)?.ok_or_else(|| {
                                let msg = "expected number or string coercible to number";
                                Error::from_luau_conversion(ty, stringify!($x), msg.to_string())
                            })?)
                        }
                    }
                })
                .ok_or_else(|| {
                    Error::from_luau_conversion(ty, stringify!($x), "out of range".to_string())
                })
            }

            fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
                let lua = ctx.lua;
                let state = lua.state();
                // SAFETY: ctx proves `idx` is valid; lua_type/tointegerx/tointeger64 are reads.
                unsafe {
                    let type_id = ffi::lua_type(state, idx);
                    if type_id == ffi::LUA_TNUMBER {
                        let mut ok = 0;
                        let i = ffi::lua_tointegerx(state, idx, &mut ok);
                        if ok != 0 {
                            return cast(i).ok_or_else(|| {
                                Error::from_luau_conversion(
                                    "integer",
                                    stringify!($x),
                                    "out of range".to_string(),
                                )
                            });
                        }
                    }
                    if type_id == ffi::LUA_TINTEGER {
                        let i = ffi::lua_tointeger64(state, idx, std::ptr::null_mut());
                        return cast(i).ok_or_else(|| {
                            Error::from_luau_conversion(
                                "integer",
                                stringify!($x),
                                "out of range".to_string(),
                            )
                        });
                    }
                    // Fallback to default
                    Self::from_luau(lua.stack_value(idx, Some(type_id)), lua.lua())
                }
            }
        }
    };
}

lua_convert_int!(i8);
lua_convert_int!(u8);
lua_convert_int!(i16);
lua_convert_int!(u16);
lua_convert_int!(i32);
lua_convert_int!(u32);
lua_convert_int!(i64);
lua_convert_int!(u64);
lua_convert_int!(i128);
lua_convert_int!(u128);
lua_convert_int!(isize);
lua_convert_int!(usize);

macro_rules! lua_convert_float {
    ($x:ty) => {
        impl IntoLuau for $x {
            #[inline]
            fn into_luau(self, _: &Luau) -> Result<Value> {
                Ok(Value::Number(self as _))
            }
        }

        impl FromLuau for $x {
            #[inline]
            fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
                let ty = value.type_name();
                value.coerce_number(lua)?.map(|n| n as $x).ok_or_else(|| {
                    let msg = "expected number or string coercible to number";
                    Error::from_luau_conversion(ty, stringify!($x), msg.to_string())
                })
            }

            fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
                let lua = ctx.lua;
                let state = lua.state();
                // SAFETY: ctx proves `idx` is valid; lua_type/tonumber are reads.
                unsafe {
                    let type_id = ffi::lua_type(state, idx);
                    if type_id == ffi::LUA_TNUMBER {
                        return Ok(ffi::lua_tonumber(state, idx) as _);
                    }
                    // Fallback to default
                    Self::from_luau(lua.stack_value(idx, Some(type_id)), lua.lua())
                }
            }
        }
    };
}

lua_convert_float!(f32);
lua_convert_float!(f64);

impl<T> IntoLuau for &[T]
where
    T: IntoLuau + Clone,
{
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::Table(
            lua.create_sequence_from(self.iter().cloned())?,
        ))
    }
}

impl<T, const N: usize> IntoLuau for [T; N]
where
    T: IntoLuau,
{
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::Table(lua.create_sequence_from(self)?))
    }
}

impl<T, const N: usize> FromLuau for [T; N]
where
    T: FromLuau,
{
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        match value {
            // SAFETY: an array of MaybeUninit<T> is itself init regardless of T; we then write
            // each slot before transmute_copy reads them as initialised. N == 3 is enforced by
            // the guard, matching Vector::SIZE.
            #[rustfmt::skip]
            Value::Vector(v) if N == crate::Vector::SIZE => unsafe {
                use std::{mem, ptr};
                let mut arr: [mem::MaybeUninit<T>; N] = mem::MaybeUninit::uninit().assume_init();
                ptr::write(arr[0].as_mut_ptr() , T::from_luau(Value::Number(v.x() as _), lua)?);
                ptr::write(arr[1].as_mut_ptr(), T::from_luau(Value::Number(v.y() as _), lua)?);
                ptr::write(arr[2].as_mut_ptr(), T::from_luau(Value::Number(v.z() as _), lua)?);
                Ok(mem::transmute_copy(&arr))
            },
            Value::Table(table) => {
                let vec = table.sequence_values().collect::<Result<Vec<_>>>()?;
                vec.try_into().map_err(|vec: Vec<T>| {
                    let msg = format!("expected table of length {N}, got {}", vec.len());
                    Error::from_luau_conversion("table", Self::type_name(), msg)
                })
            }
            _ => {
                let msg = format!("expected table of length {N}");
                let err = Error::from_luau_conversion(value.type_name(), Self::type_name(), msg);
                Err(err)
            }
        }
    }
}

impl<T: IntoLuau> IntoLuau for Box<[T]> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::Table(lua.create_sequence_from(self.into_vec())?))
    }
}

impl<T: FromLuau> FromLuau for Box<[T]> {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        Ok(Vec::<T>::from_luau(value, lua)?.into_boxed_slice())
    }
}

impl<T: IntoLuau> IntoLuau for Vec<T> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::Table(lua.create_sequence_from(self)?))
    }
}

impl<T: FromLuau> FromLuau for Vec<T> {
    #[inline]
    fn from_luau(value: Value, _lua: &Luau) -> Result<Self> {
        match value {
            Value::Table(table) => table.sequence_values().collect(),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                Self::type_name(),
                "expected table".to_string(),
            )),
        }
    }
}

impl<K: Eq + Hash + IntoLuau, V: IntoLuau, S: BuildHasher> IntoLuau for HashMap<K, V, S> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::Table(lua.create_table_from(self)?))
    }
}

impl<K: Eq + Hash + FromLuau, V: FromLuau, S: BuildHasher + Default> FromLuau for HashMap<K, V, S> {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::Table(table) => table.pairs().collect(),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                Self::type_name(),
                "expected table".to_string(),
            )),
        }
    }
}

impl<K: Ord + IntoLuau, V: IntoLuau> IntoLuau for BTreeMap<K, V> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::Table(lua.create_table_from(self)?))
    }
}

impl<K: Ord + FromLuau, V: FromLuau> FromLuau for BTreeMap<K, V> {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::Table(table) => table.pairs().collect(),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                Self::type_name(),
                "expected table".to_string(),
            )),
        }
    }
}

impl<T: Eq + Hash + IntoLuau, S: BuildHasher> IntoLuau for HashSet<T, S> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::Table(lua.create_table_from(
            self.into_iter().map(|val| (val, true)),
        )?))
    }
}

impl<T: Eq + Hash + FromLuau, S: BuildHasher + Default> FromLuau for HashSet<T, S> {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::Table(table) if table.raw_len() > 0 => table.sequence_values().collect(),
            Value::Table(table) => table
                .pairs::<T, Value>()
                .map(|res| res.map(|(k, _)| k))
                .collect(),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                Self::type_name(),
                "expected table".to_string(),
            )),
        }
    }
}

impl<T: Ord + IntoLuau> IntoLuau for BTreeSet<T> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        Ok(Value::Table(lua.create_table_from(
            self.into_iter().map(|val| (val, true)),
        )?))
    }
}

impl<T: Ord + FromLuau> FromLuau for BTreeSet<T> {
    #[inline]
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::Table(table) if table.raw_len() > 0 => table.sequence_values().collect(),
            Value::Table(table) => table
                .pairs::<T, Value>()
                .map(|res| res.map(|(k, _)| k))
                .collect(),
            _ => Err(Error::from_luau_conversion(
                value.type_name(),
                Self::type_name(),
                "expected table".to_string(),
            )),
        }
    }
}

impl<T: IntoLuau> IntoLuau for Option<T> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        match self {
            Some(val) => val.into_luau(lua),
            None => Ok(Nil),
        }
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        match self {
            Some(val) => val.push_into_stack(ctx)?,
            // SAFETY: ctx proves stack reservation; lua_pushnil cannot raise.
            None => unsafe { ffi::lua_pushnil(ctx.lua.state()) },
        }
        Ok(())
    }
}

impl<T: FromLuau> FromLuau for Option<T> {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        match value {
            Nil => Ok(None),
            value => Ok(Some(T::from_luau(value, lua)?)),
        }
    }

    #[inline]
    fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        // SAFETY: ctx proves `idx` is valid; lua_type is a pure read.
        match unsafe { ffi::lua_type(ctx.lua.state(), idx) } {
            ffi::LUA_TNIL => Ok(None),
            _ => Ok(Some(T::from_stack(idx, ctx)?)),
        }
    }
}

impl<L: IntoLuau, R: IntoLuau> IntoLuau for Either<L, R> {
    #[inline]
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        match self {
            Self::Left(l) => l.into_luau(lua),
            Self::Right(r) => r.into_luau(lua),
        }
    }

    #[inline]
    fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        match self {
            Self::Left(l) => l.push_into_stack(ctx),
            Self::Right(r) => r.push_into_stack(ctx),
        }
    }
}

impl<L: FromLuau, R: FromLuau> FromLuau for Either<L, R> {
    #[inline]
    fn from_luau(value: Value, lua: &Luau) -> Result<Self> {
        let value_type_name = value.type_name();
        // Try the left type first
        match L::from_luau(value.clone(), lua) {
            Ok(l) => Ok(Self::Left(l)),
            // Try the right type
            Err(_) => match R::from_luau(value, lua).map(Either::Right) {
                Ok(r) => Ok(r),
                Err(_) => Err(Error::from_luau_conversion(
                    value_type_name,
                    Self::type_name(),
                    None,
                )),
            },
        }
    }

    #[inline]
    fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        match L::from_stack(idx, ctx) {
            Ok(l) => Ok(Self::Left(l)),
            Err(_) => match R::from_stack(idx, ctx).map(Either::Right) {
                Ok(r) => Ok(r),
                Err(_) => {
                    let state = ctx.lua.state();
                    // SAFETY: ctx proves `idx` is valid; lua_type returns a tag, lua_typename
                    // returns a static C string.
                    let from_type_name = unsafe {
                        CStr::from_ptr(ffi::lua_typename(state, ffi::lua_type(state, idx)))
                            .to_str()
                            .unwrap_or("unknown")
                    };
                    let err = Error::from_luau_conversion(from_type_name, Self::type_name(), None);
                    Err(err)
                }
            },
        }
    }
}
