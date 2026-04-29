use std::{
    cell::UnsafeCell,
    os::raw::{c_int, c_void},
};

pub use sync::XRc;

use crate::{
    error::Result,
    state::{ExtraData, Luau, RawLuau},
};

// The boxed future boundary is still needed for async callback trait objects and poll upvalues.
type BoxFuture<'a, T> = futures_util::future::LocalBoxFuture<'a, T>;

pub use app_data::{AppData, AppDataRef, AppDataRefMut};
pub use either::Either;
pub use registry_key::RegistryKey;
pub use value_ref::{ValueRef, ValueRefIndex};

/// Type of Luau integer numbers.
pub type Integer = ffi::lua_Integer;
/// Type of Luau floating point numbers.
pub type Number = ffi::lua_Number;

/// A "light" userdata value. Equivalent to an unmanaged raw pointer.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct LightUserData(pub *mut c_void);

type CallbackFn<'a> = dyn Fn(&RawLuau, c_int) -> Result<c_int> + 'a;

pub type Callback = Box<CallbackFn<'static>>;
pub type CallbackPtr = *const CallbackFn<'static>;

pub type ScopedCallback<'s> = Box<dyn Fn(&RawLuau, c_int) -> Result<c_int> + 's>;

pub struct Upvalue<T> {
    pub(crate) data: T,
    pub(crate) extra: XRc<UnsafeCell<ExtraData>>,
}

pub type CallbackUpvalue = Upvalue<Option<Callback>>;

pub type AsyncCallback =
    Box<dyn for<'a> Fn(&'a RawLuau, c_int) -> BoxFuture<'a, Result<c_int>> + 'static>;
pub type AsyncCallbackUpvalue = Upvalue<AsyncCallback>;
pub type AsyncPollUpvalue = Upvalue<Option<BoxFuture<'static, Result<c_int>>>>;

/// Type to set next Luau VM action after executing interrupt or hook function.
pub enum VmState {
    /// Continue VM execution.
    Continue,
    /// Yield the current thread.
    ///
    /// Supported by Luau.
    Yield,
}

pub type InterruptCallback = XRc<dyn Fn(&Luau) -> Result<VmState>>;

pub type ThreadCreationCallback = XRc<dyn Fn(&Luau, crate::Thread) -> Result<()>>;

pub type ThreadCollectionCallback = XRc<dyn Fn(crate::LightUserData)>;

pub struct DestructedUserdata;

pub trait LuauType {
    const TYPE_ID: c_int;
}

impl LuauType for bool {
    const TYPE_ID: c_int = ffi::LUA_TBOOLEAN;
}

impl LuauType for Number {
    const TYPE_ID: c_int = ffi::LUA_TNUMBER;
}

impl LuauType for LightUserData {
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
