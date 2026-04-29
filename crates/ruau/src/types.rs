use std::{
    cell::UnsafeCell,
    os::raw::{c_int, c_void},
};

pub use sync::XRc;

use crate::{
    error::Result,
    state::{ExtraData, Lua, RawLua},
};

pub type BoxFuture<'a, T> = futures_util::future::LocalBoxFuture<'a, T>;

pub use app_data::{AppData, AppDataRef, AppDataRefMut};
pub use either::Either;
pub use registry_key::RegistryKey;
pub use value_ref::{ValueRef, ValueRefIndex};

/// Type of Lua integer numbers.
pub type Integer = ffi::lua_Integer;
/// Type of Lua floating point numbers.
pub type Number = ffi::lua_Number;

/// A "light" userdata value. Equivalent to an unmanaged raw pointer.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct LightUserData(pub *mut c_void);

type CallbackFn<'a> = dyn Fn(&RawLua, c_int) -> Result<c_int> + 'a;

pub type Callback = Box<CallbackFn<'static>>;
pub type CallbackPtr = *const CallbackFn<'static>;

pub type ScopedCallback<'s> = Box<dyn Fn(&RawLua, c_int) -> Result<c_int> + 's>;

pub struct Upvalue<T> {
    pub(crate) data: T,
    pub(crate) extra: XRc<UnsafeCell<ExtraData>>,
}

pub type CallbackUpvalue = Upvalue<Option<Callback>>;

pub type AsyncCallback =
    Box<dyn for<'a> Fn(&'a RawLua, c_int) -> BoxFuture<'a, Result<c_int>> + 'static>;
pub type AsyncCallbackUpvalue = Upvalue<AsyncCallback>;
pub type AsyncPollUpvalue = Upvalue<Option<BoxFuture<'static, Result<c_int>>>>;

/// Type to set next Lua VM action after executing interrupt or hook function.
pub enum VmState {
    /// Continue VM execution.
    Continue,
    /// Yield the current thread.
    ///
    /// Supported by Lua 5.3+ and Luau.
    Yield,
}

pub type InterruptCallback = XRc<dyn Fn(&Lua) -> Result<VmState>>;

pub type ThreadCreationCallback = XRc<dyn Fn(&Lua, crate::Thread) -> Result<()>>;

pub type ThreadCollectionCallback = XRc<dyn Fn(crate::LightUserData)>;

pub struct DestructedUserdata;

pub trait LuaType {
    const TYPE_ID: c_int;
}

impl LuaType for bool {
    const TYPE_ID: c_int = ffi::LUA_TBOOLEAN;
}

impl LuaType for Number {
    const TYPE_ID: c_int = ffi::LUA_TNUMBER;
}

impl LuaType for LightUserData {
    const TYPE_ID: c_int = ffi::LUA_TLIGHTUSERDATA;
}

mod app_data;
mod registry_key;
mod sync;
mod value_ref;

#[cfg(test)]
mod assertions {
    use super::*;

    static_assertions::assert_not_impl_any!(ValueRef: Send);
}
