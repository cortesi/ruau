//! Shared raw-handle, callback, and VM support types.

use std::{
    cell::UnsafeCell,
    os::raw::{c_int, c_void},
};

use futures_util::future::LocalBoxFuture;
pub use sync::XRc;

use crate::{
    error::Result,
    state::{ExtraData, Luau, RawLuau},
};

/// Boxed future boundary used by async callback trait objects and poll upvalues.
type BoxFuture<'a, T> = LocalBoxFuture<'a, T>;

pub use app_data::{AppData, AppDataRef, AppDataRefMut};
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
pub type ThreadCollectionCallback = XRc<dyn Fn(LightUserData)>;

/// Marker left behind when userdata storage has already been destroyed.
pub struct DestructedUserdata;

/// Built-in Luau value kind with a shared primitive metatable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PrimitiveType {
    /// Luau boolean.
    Boolean,
    /// Luau number.
    Number,
    /// Luau light userdata.
    LightUserData,
    /// Luau vector.
    Vector,
    /// Luau string.
    String,
    /// Luau function.
    Function,
    /// Luau thread.
    Thread,
    /// Luau buffer.
    Buffer,
}

impl PrimitiveType {
    pub(crate) const fn type_id(self) -> c_int {
        match self {
            Self::Boolean => ffi::LUA_TBOOLEAN,
            Self::Number => ffi::LUA_TNUMBER,
            Self::LightUserData => ffi::LUA_TLIGHTUSERDATA,
            Self::Vector => ffi::LUA_TVECTOR,
            Self::String => ffi::LUA_TSTRING,
            Self::Function => ffi::LUA_TFUNCTION,
            Self::Thread => ffi::LUA_TTHREAD,
            Self::Buffer => ffi::LUA_TBUFFER,
        }
    }
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
