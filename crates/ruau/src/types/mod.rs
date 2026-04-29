//! Shared raw-handle, callback, and VM support types.

use std::{
    cell::UnsafeCell,
    os::raw::{c_int, c_void},
};

pub use sync::XRc;

use crate::{
    error::Result,
    state::{ExtraData, Luau, RawLuau},
};

/// Boxed future boundary used by async callback trait objects and poll upvalues.
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

/// Raw callback signature used by Rust functions registered with Luau.
type CallbackFn<'a> = dyn Fn(&RawLuau, c_int) -> Result<c_int> + 'a;

/// Owned callback stored in Luau upvalues.
pub type Callback = Box<CallbackFn<'static>>;
/// Raw pointer to a callback stored in a Luau upvalue.
pub type CallbackPtr = *const CallbackFn<'static>;

/// Callback that may borrow values from a [`Scope`](crate::Scope).
pub type ScopedCallback<'s> = Box<dyn Fn(&RawLuau, c_int) -> Result<c_int> + 's>;

/// Data paired with the owning VM's extra state for Luau upvalue storage.
pub struct Upvalue<T> {
    /// Rust value stored behind the Luau upvalue.
    pub(crate) data: T,
    /// Extra state for the VM that owns the upvalue.
    pub(crate) extra: XRc<UnsafeCell<ExtraData>>,
}

/// Upvalue storage for synchronous Rust callbacks.
pub type CallbackUpvalue = Upvalue<Option<Callback>>;

/// Owned async callback stored in Luau upvalues.
pub type AsyncCallback =
    Box<dyn for<'a> Fn(&'a RawLuau, c_int) -> BoxFuture<'a, Result<c_int>> + 'static>;
/// Upvalue storage for async Rust callbacks.
pub type AsyncCallbackUpvalue = Upvalue<AsyncCallback>;
/// Upvalue storage for an in-flight async callback poll.
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

/// Interrupt callback installed on a Luau state.
pub type InterruptCallback = XRc<dyn Fn(&Luau) -> Result<VmState>>;

/// Hook invoked after a Luau thread is created.
pub type ThreadCreationCallback = XRc<dyn Fn(&Luau, crate::Thread) -> Result<()>>;

/// Hook invoked when a Luau thread is collected.
pub type ThreadCollectionCallback = XRc<dyn Fn(crate::LightUserData)>;

/// Marker left behind when userdata storage has already been destroyed.
pub struct DestructedUserdata;

/// Maps Rust marker types to Luau runtime type IDs.
pub trait LuauType {
    /// Luau `LUA_T*` type tag represented by the Rust type.
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
