use std::cell::RefCell;

use super::{
    lock::{RawLock, RwLock, UserDataLock},
    r#ref::{UserDataRef, UserDataRefMut},
};
use crate::{
    error::{Error, Result},
    types::XRc,
};

pub(crate) enum UserDataStorage<T> {
    Owned(UserDataVariant<T>),
    Scoped(ScopedUserDataVariant<T>),
}

// A shared container for owned userdata values.
pub struct UserDataVariant<T>(XRc<RwLock<T>>);

impl<T> Clone for UserDataVariant<T> {
    #[inline]
    fn clone(&self) -> Self {
        Self(XRc::clone(&self.0))
    }
}

impl<T> UserDataVariant<T> {
    #[inline(always)]
    pub(super) fn try_borrow_scoped<R>(&self, f: impl FnOnce(&T) -> R) -> Result<R> {
        // Shared borrow tracking is sufficient here: owned userdata is only accessed through the
        // single-owner Luau state, and Luau execution does not share a live userdata value across
        // threads.
        let _guard = (self.raw_lock().try_lock_shared_guarded()).map_err(|_| Error::UserDataBorrowError)?;
        Ok(f(unsafe { &*self.as_ptr() }))
    }

    // Mutably borrows the wrapped value in-place.
    #[inline(always)]
    fn try_borrow_scoped_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        let _guard =
            (self.raw_lock().try_lock_exclusive_guarded()).map_err(|_| Error::UserDataBorrowMutError)?;
        Ok(f(unsafe { &mut *self.as_ptr() }))
    }

    // Immutably borrows the wrapped value and returns an owned reference.
    #[inline(always)]
    fn try_borrow_owned(&self) -> Result<UserDataRef<T>> {
        UserDataRef::try_from(self.clone())
    }

    // Mutably borrows the wrapped value and returns an owned reference.
    #[inline(always)]
    fn try_borrow_owned_mut(&self) -> Result<UserDataRefMut<T>> {
        UserDataRefMut::try_from(self.clone())
    }

    // Returns the wrapped value.
    //
    // This method checks that we have exclusive access to the value.
    fn into_inner(self) -> Result<T> {
        if !self.raw_lock().try_lock_exclusive() {
            return Err(Error::UserDataBorrowMutError);
        }
        Ok(XRc::into_inner(self.0).unwrap().into_inner())
    }

    #[inline(always)]
    fn strong_count(&self) -> usize {
        XRc::strong_count(&self.0)
    }

    #[inline(always)]
    pub(super) fn raw_lock(&self) -> &RawLock {
        unsafe { self.0.raw() }
    }

    #[inline(always)]
    pub(super) fn as_ptr(&self) -> *mut T {
        self.0.data_ptr()
    }
}

pub enum ScopedUserDataVariant<T> {
    Ref(*const T),
    RefMut(RefCell<*mut T>),
    Boxed(RefCell<*mut T>),
}

impl<T> Drop for ScopedUserDataVariant<T> {
    #[inline]
    fn drop(&mut self) {
        if let Self::Boxed(value) = self
            && let Ok(value) = value.try_borrow_mut()
        {
            unsafe { drop(Box::from_raw(*value)) }
        }
    }
}

impl<T: 'static> UserDataStorage<T> {
    #[inline(always)]
    pub(crate) fn new(data: T) -> Self {
        Self::Owned(UserDataVariant(XRc::new(RwLock::new(data))))
    }

    #[inline(always)]
    pub(crate) fn new_ref(data: &T) -> Self {
        Self::Scoped(ScopedUserDataVariant::Ref(data))
    }

    #[inline(always)]
    pub(crate) fn new_ref_mut(data: &mut T) -> Self {
        Self::Scoped(ScopedUserDataVariant::RefMut(RefCell::new(data)))
    }

    // Immutably borrows the wrapped value and returns an owned reference.
    #[inline(always)]
    pub(crate) fn try_borrow_owned(&self) -> Result<UserDataRef<T>> {
        match self {
            Self::Owned(data) => data.try_borrow_owned(),
            Self::Scoped(_) => Err(Error::UserDataTypeMismatch),
        }
    }

    // Mutably borrows the wrapped value and returns an owned reference.
    #[inline(always)]
    pub(crate) fn try_borrow_owned_mut(&self) -> Result<UserDataRefMut<T>> {
        match self {
            Self::Owned(data) => data.try_borrow_owned_mut(),
            Self::Scoped(_) => Err(Error::UserDataTypeMismatch),
        }
    }

    #[inline(always)]
    pub(crate) fn into_inner(self) -> Result<T> {
        match self {
            Self::Owned(data) => data.into_inner(),
            Self::Scoped(_) => Err(Error::UserDataTypeMismatch),
        }
    }
}

impl<T> UserDataStorage<T> {
    #[inline(always)]
    pub(crate) fn new_scoped(data: T) -> Self {
        let data = Box::into_raw(Box::new(data));
        Self::Scoped(ScopedUserDataVariant::Boxed(RefCell::new(data)))
    }

    /// Returns `true` if it's safe to destroy the container.
    ///
    /// It's safe to destroy the container if the reference count is greater than 1 or the lock is
    /// not acquired.
    #[inline(always)]
    pub(crate) fn is_safe_to_destroy(&self) -> bool {
        match self {
            Self::Owned(variant) => variant.strong_count() > 1 || !variant.raw_lock().is_locked(),
            Self::Scoped(_) => false,
        }
    }

    /// Returns `true` if the container has exclusive access to the value.
    #[inline(always)]
    pub(crate) fn has_exclusive_access(&self) -> bool {
        match self {
            Self::Owned(variant) => !variant.raw_lock().is_locked(),
            Self::Scoped(_) => false,
        }
    }

    #[inline]
    pub(crate) fn try_borrow_scoped<R>(&self, f: impl FnOnce(&T) -> R) -> Result<R> {
        match self {
            Self::Owned(data) => data.try_borrow_scoped(f),
            Self::Scoped(ScopedUserDataVariant::Ref(value)) => Ok(f(unsafe { &**value })),
            Self::Scoped(ScopedUserDataVariant::RefMut(value) | ScopedUserDataVariant::Boxed(value)) => {
                let t = value.try_borrow().map_err(|_| Error::UserDataBorrowError)?;
                Ok(f(unsafe { &**t }))
            }
        }
    }

    #[inline]
    pub(crate) fn try_borrow_scoped_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        match self {
            Self::Owned(data) => data.try_borrow_scoped_mut(f),
            Self::Scoped(ScopedUserDataVariant::Ref(_)) => Err(Error::UserDataBorrowMutError),
            Self::Scoped(ScopedUserDataVariant::RefMut(value) | ScopedUserDataVariant::Boxed(value)) => {
                let mut t = value
                    .try_borrow_mut()
                    .map_err(|_| Error::UserDataBorrowMutError)?;
                Ok(f(unsafe { &mut **t }))
            }
        }
    }
}
