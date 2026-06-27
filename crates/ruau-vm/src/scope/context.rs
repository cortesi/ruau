use std::{
    any::{Any, TypeId},
    cell::{RefCell, RefMut},
    collections::HashMap,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

/// Typed host state, one value per Rust type. Kept in its **own** cell, separate
/// from the heap, so a `Scope::app_data` read never collides with the heap borrow
/// a step holds for value construction (a host can read its config while building
/// a table). Only a held `app_data` + `app_data_mut` on the *same* cell conflict —
/// the ordinary `RefCell` discipline.
#[derive(Default)]
pub struct AppData(HashMap<TypeId, Box<dyn Any + Send + Sync>>);

impl AppData {
    pub(crate) fn set<T: Any + Send + Sync>(&mut self, value: T) {
        self.0.insert(TypeId::of::<T>(), Box::new(value));
    }

    pub(crate) fn set_boxed(&mut self, value: Box<dyn Any + Send + Sync>) {
        self.0.insert(value.as_ref().type_id(), value);
    }

    pub(crate) fn get<T: Any>(&self) -> Option<&T> {
        self.0
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref())
    }

    pub(crate) fn get_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.0
            .get_mut(&TypeId::of::<T>())
            .and_then(|v| v.downcast_mut())
    }

    pub(crate) fn remove<T: Any>(&mut self) -> bool {
        self.0.remove(&TypeId::of::<T>()).is_some()
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

/// Borrowed non-`Send` host context lent to one VM entry.
pub struct ContextSlot {
    type_id: TypeId,
    value: NonNull<()>,
    borrow: RefCell<()>,
}

impl ContextSlot {
    pub(crate) fn new<T: Any>(value: &mut T) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            value: NonNull::from(value).cast(),
            borrow: RefCell::new(()),
        }
    }

    pub(super) fn borrow_mut<T: Any>(&self) -> Option<ContextMut<'_, T>> {
        if self.type_id != TypeId::of::<T>() {
            return None;
        }
        let borrow = self.borrow.try_borrow_mut().ok()?;
        Some(ContextMut {
            value: self.value.cast(),
            _borrow: borrow,
            _marker: PhantomData,
        })
    }
}

/// Mutable guard for the borrowed host context active on this VM entry.
pub struct ContextMut<'a, T: Any> {
    value: NonNull<T>,
    _borrow: RefMut<'a, ()>,
    _marker: PhantomData<&'a mut T>,
}

impl<T: Any> Deref for ContextMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the `ContextSlot` was built from a live `&mut T`, and this
        // guard holds the slot's mutable borrow token.
        unsafe { self.value.as_ref() }
    }
}

impl<T: Any> DerefMut for ContextMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the `ContextSlot` was built from a live `&mut T`, and this
        // guard holds the slot's mutable borrow token.
        unsafe { self.value.as_mut() }
    }
}
