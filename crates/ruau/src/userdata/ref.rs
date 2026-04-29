use std::{
    any::{TypeId, type_name},
    fmt, mem,
    ops::{Deref, DerefMut},
    os::raw::c_int,
};

use super::{
    cell::{UserDataStorage, UserDataVariant},
    lock::{LockGuard, RawLock, UserDataLock},
};
use crate::{
    error::{Error, Result},
    state::{Luau, RawLuau},
    traits::{FromLuau, StackCtx},
    userdata::AnyUserData,
    util::{check_stack, get_userdata, take_userdata},
    value::Value,
};

/// A wrapper type for a userdata value that provides read access.
///
/// It implements [`FromLuau`] and can be used to receive a typed userdata from Luau.
pub struct UserDataRef<T: 'static> {
    // It's important to drop the guard first, as it refers to the `inner` data.
    _guard: LockGuard<'static, RawLock>,
    inner: UserDataVariant<T>,
}

impl<T> Deref for UserDataRef<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.inner.as_ptr() }
    }
}

impl<T: fmt::Debug> fmt::Debug for UserDataRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: fmt::Display> fmt::Display for UserDataRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> TryFrom<UserDataVariant<T>> for UserDataRef<T> {
    type Error = Error;

    #[inline]
    fn try_from(variant: UserDataVariant<T>) -> Result<Self> {
        let guard = variant.raw_lock().try_lock_shared_guarded();
        let guard = guard.map_err(|_| Error::UserDataBorrowError)?;
        let guard = unsafe { mem::transmute::<LockGuard<_>, LockGuard<'static, _>>(guard) };
        Ok(Self {
            _guard: guard,
            inner: variant,
        })
    }
}

impl<T: 'static> FromLuau for UserDataRef<T> {
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        try_value_to_userdata::<T>(value)?.borrow()
    }

    #[inline]
    unsafe fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let lua = ctx.lua;
        Self::borrow_from_stack(lua, lua.state(), idx)
    }
}

impl<T: 'static> UserDataRef<T> {
    pub(crate) unsafe fn borrow_from_stack(
        lua: &RawLuau,
        state: *mut ffi::lua_State,
        idx: c_int,
    ) -> Result<Self> {
        let type_id = lua.get_userdata_type_id::<T>(state, idx)?;
        match type_id {
            Some(type_id) if type_id == TypeId::of::<T>() => {
                let ud = get_userdata::<UserDataStorage<T>>(state, idx);
                (*ud).try_borrow_owned()
            }

            _ => Err(Error::UserDataTypeMismatch),
        }
    }
}

/// A wrapper type for a userdata value that provides read and write access.
///
/// It implements [`FromLuau`] and can be used to receive a typed userdata from Luau.
pub struct UserDataRefMut<T: 'static> {
    // It's important to drop the guard first, as it refers to the `inner` data.
    _guard: LockGuard<'static, RawLock>,
    inner: UserDataVariant<T>,
}

impl<T> Deref for UserDataRefMut<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner.as_ptr() }
    }
}

impl<T> DerefMut for UserDataRefMut<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.inner.as_ptr() }
    }
}

impl<T: fmt::Debug> fmt::Debug for UserDataRefMut<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: fmt::Display> fmt::Display for UserDataRefMut<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> TryFrom<UserDataVariant<T>> for UserDataRefMut<T> {
    type Error = Error;

    #[inline]
    fn try_from(variant: UserDataVariant<T>) -> Result<Self> {
        let guard = variant.raw_lock().try_lock_exclusive_guarded();
        let guard = guard.map_err(|_| Error::UserDataBorrowMutError)?;
        let guard = unsafe { mem::transmute::<LockGuard<_>, LockGuard<'static, _>>(guard) };
        Ok(Self {
            _guard: guard,
            inner: variant,
        })
    }
}

impl<T: 'static> FromLuau for UserDataRefMut<T> {
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        try_value_to_userdata::<T>(value)?.borrow_mut()
    }

    unsafe fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let lua = ctx.lua;
        Self::borrow_from_stack(lua, lua.state(), idx)
    }
}

impl<T: 'static> UserDataRefMut<T> {
    pub(crate) unsafe fn borrow_from_stack(
        lua: &RawLuau,
        state: *mut ffi::lua_State,
        idx: c_int,
    ) -> Result<Self> {
        let type_id = lua.get_userdata_type_id::<T>(state, idx)?;
        match type_id {
            Some(type_id) if type_id == TypeId::of::<T>() => {
                let ud = get_userdata::<UserDataStorage<T>>(state, idx);
                (*ud).try_borrow_owned_mut()
            }

            _ => Err(Error::UserDataTypeMismatch),
        }
    }
}

/// A wrapper type that takes ownership of a userdata value.
///
/// It implements [`FromLuau`] and can be used to receive a typed userdata from Luau by taking
/// ownership of it.
/// The original Luau userdata is marked as destructed and cannot be used further.
pub struct UserDataOwned<T>(pub T);

impl<T> Deref for UserDataOwned<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for UserDataOwned<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: fmt::Debug> fmt::Debug for UserDataOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: fmt::Display> fmt::Display for UserDataOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: 'static> FromLuau for UserDataOwned<T> {
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        try_value_to_userdata::<T>(value)?.take().map(UserDataOwned)
    }

    unsafe fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let lua = ctx.lua;
        let state = lua.state();
        let type_id = lua.get_userdata_type_id::<T>(state, idx)?;
        match type_id {
            Some(type_id) if type_id == TypeId::of::<T>() => {
                let ud = get_userdata::<UserDataStorage<T>>(state, idx);
                if (*ud).has_exclusive_access() {
                    check_stack(state, 1)?;
                    take_userdata::<UserDataStorage<T>>(state, idx)
                        .into_inner()
                        .map(UserDataOwned)
                } else {
                    Err(Error::UserDataBorrowMutError)
                }
            }
            _ => Err(Error::UserDataTypeMismatch),
        }
    }
}

#[inline]
fn try_value_to_userdata<T>(value: Value) -> Result<AnyUserData> {
    match value {
        Value::UserData(ud) => Ok(ud),
        _ => Err(Error::from_luau_conversion(
            value.type_name(),
            "userdata",
            format!("expected userdata of type {}", type_name::<T>()),
        )),
    }
}

#[cfg(test)]
mod assertions {
    use super::*;

    static_assertions::assert_not_impl_all!(UserDataRef<()>: Send, Sync);
    static_assertions::assert_not_impl_all!(UserDataRefMut<()>: Send, Sync);
}
