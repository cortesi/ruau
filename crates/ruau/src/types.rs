use std::{
    cell::UnsafeCell,
    os::raw::{c_int, c_void},
};

// Re-export mutex wrappers
pub use sync::{ArcReentrantMutexGuard, ReentrantMutex, ReentrantMutexGuard, XRc, XWeak};

use crate::{
    error::Result,
    state::{ExtraData, Lua, RawLua},
};

#[cfg(all(feature = "async", feature = "send"))]
pub type BoxFuture<'a, T> = futures_util::future::BoxFuture<'a, T>;

#[cfg(all(feature = "async", not(feature = "send")))]
pub(crate) type BoxFuture<'a, T> = futures_util::future::LocalBoxFuture<'a, T>;

pub use app_data::{AppData, AppDataRef, AppDataRefMut};
pub use either::Either;
pub use registry_key::RegistryKey;
pub use value_ref::ValueRef;
#[cfg(feature = "async")]
pub use value_ref::ValueRefIndex;

/// Type of Lua integer numbers.
pub type Integer = ffi::lua_Integer;
/// Type of Lua floating point numbers.
pub type Number = ffi::lua_Number;

/// A "light" userdata value. Equivalent to an unmanaged raw pointer.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct LightUserData(pub *mut c_void);

#[cfg(feature = "send")]
unsafe impl Send for LightUserData {}
#[cfg(feature = "send")]
unsafe impl Sync for LightUserData {}

#[cfg(feature = "send")]
type CallbackFn<'a> = dyn Fn(&RawLua, c_int) -> Result<c_int> + Send + 'a;

#[cfg(not(feature = "send"))]
type CallbackFn<'a> = dyn Fn(&RawLua, c_int) -> Result<c_int> + 'a;

pub type Callback = Box<CallbackFn<'static>>;
pub type CallbackPtr = *const CallbackFn<'static>;

pub type ScopedCallback<'s> = Box<dyn Fn(&RawLua, c_int) -> Result<c_int> + 's>;

pub struct Upvalue<T> {
    pub(crate) data: T,
    pub(crate) extra: XRc<UnsafeCell<ExtraData>>,
}

pub type CallbackUpvalue = Upvalue<Option<Callback>>;

#[cfg(all(feature = "async", feature = "send"))]
pub type AsyncCallback =
    Box<dyn for<'a> Fn(&'a RawLua, c_int) -> BoxFuture<'a, Result<c_int>> + Send + 'static>;

#[cfg(all(feature = "async", not(feature = "send")))]
pub(crate) type AsyncCallback =
    Box<dyn for<'a> Fn(&'a RawLua, c_int) -> BoxFuture<'a, Result<c_int>> + 'static>;

#[cfg(feature = "async")]
pub type AsyncCallbackUpvalue = Upvalue<AsyncCallback>;

#[cfg(feature = "async")]
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

#[cfg(feature = "send")]
pub type InterruptCallback = XRc<dyn Fn(&Lua) -> Result<VmState> + Send>;

#[cfg(not(feature = "send"))]
pub(crate) type InterruptCallback = XRc<dyn Fn(&Lua) -> Result<VmState>>;

#[cfg(feature = "send")]
pub type ThreadCreationCallback = XRc<dyn Fn(&Lua, crate::Thread) -> Result<()> + Send>;

#[cfg(not(feature = "send"))]
pub(crate) type ThreadCreationCallback = XRc<dyn Fn(&Lua, crate::Thread) -> Result<()>>;

#[cfg(feature = "send")]
pub type ThreadCollectionCallback = XRc<dyn Fn(crate::LightUserData) + Send>;

#[cfg(not(feature = "send"))]
pub(crate) type ThreadCollectionCallback = XRc<dyn Fn(crate::LightUserData)>;

/// A trait that adds `Send` requirement if `send` feature is enabled.
#[cfg(feature = "send")]
pub trait MaybeSend: Send {}
#[cfg(feature = "send")]
impl<T: Send> MaybeSend for T {}

/// A trait that adds `Send` requirement if `send` feature is enabled.
#[cfg(not(feature = "send"))]
pub trait MaybeSend {}
#[cfg(not(feature = "send"))]
impl<T> MaybeSend for T {}

/// A trait that adds `Sync` requirement if `send` feature is enabled.
#[cfg(feature = "send")]
pub trait MaybeSync: Sync {}
#[cfg(feature = "send")]
impl<T: Sync> MaybeSync for T {}

/// A trait that adds `Sync` requirement if `send` feature is enabled.
#[cfg(not(feature = "send"))]
pub trait MaybeSync {}
#[cfg(not(feature = "send"))]
impl<T> MaybeSync for T {}

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

    #[cfg(not(feature = "send"))]
    static_assertions::assert_not_impl_any!(ValueRef: Send);
    #[cfg(feature = "send")]
    static_assertions::assert_impl_all!(ValueRef: Send, Sync);
}
