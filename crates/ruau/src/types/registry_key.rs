use std::{
    cell::RefCell,
    fmt,
    hash::{Hash, Hasher},
    mem,
    os::raw::c_int,
    ptr,
    rc::Rc,
};

/// An auto generated key into the Luau registry.
///
/// This is a handle to a value stored inside the Luau registry. It is not automatically
/// garbage collected on Drop, but it can be removed with [`Registry::remove`], and
/// instances not manually removed can be garbage collected with [`Registry::expire`].
///
/// Be warned, If you place this into Luau via a [`UserData`] type or a Rust callback, it is *easy*
/// to accidentally cause reference cycles that the Luau garbage collector cannot resolve. Instead of
/// placing a [`RegistryKey`] into a [`UserData`] type, consider to use
/// [`AnyUserData::set_user_value`].
///
/// [`UserData`]: crate::UserData
/// [`RegistryKey`]: crate::RegistryKey
/// [`Registry::remove`]: crate::Registry::remove
/// [`Registry::expire`]: crate::Registry::expire
/// [`AnyUserData::set_user_value`]: crate::AnyUserData::set_user_value
pub struct RegistryKey {
    pub(crate) registry_id: i32,
    pub(crate) unref_list: Rc<RefCell<Option<Vec<c_int>>>>,
}

impl fmt::Debug for RegistryKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RegistryKey({})", self.id())
    }
}

impl Hash for RegistryKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id().hash(state)
    }
}

impl PartialEq for RegistryKey {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id() && Rc::ptr_eq(&self.unref_list, &other.unref_list)
    }
}

impl Eq for RegistryKey {}

impl Drop for RegistryKey {
    fn drop(&mut self) {
        let registry_id = self.id();
        // We don't need to collect nil slot
        if registry_id > ffi::LUA_REFNIL
            && let Some(list) = self.unref_list.borrow_mut().as_mut()
        {
            list.push(registry_id);
        }
    }
}

impl RegistryKey {
    /// Creates a new instance of `RegistryKey`
    pub(crate) const fn new(id: c_int, unref_list: Rc<RefCell<Option<Vec<c_int>>>>) -> Self {
        Self {
            registry_id: id,
            unref_list,
        }
    }

    /// Returns the underlying Luau reference of this `RegistryKey`
    #[inline(always)]
    pub fn id(&self) -> c_int {
        self.registry_id
    }

    /// Sets the unique Luau reference key of this `RegistryKey`
    #[inline(always)]
    pub(crate) fn set_id(&mut self, id: c_int) {
        self.registry_id = id;
    }

    /// Destroys the `RegistryKey` without adding to the unref list
    pub(crate) fn take(self) -> i32 {
        let registry_id = self.id();
        // SAFETY: read the Rc out of `self` (avoiding double-drop) and forget the rest of
        // the struct so `Drop for RegistryKey` does not push into the unref list.
        unsafe {
            ptr::read(&self.unref_list);
            mem::forget(self);
        }
        registry_id
    }
}

#[cfg(test)]
mod assertions {
    use super::*;

    static_assertions::assert_not_impl_any!(RegistryKey: Send, Sync);
}
