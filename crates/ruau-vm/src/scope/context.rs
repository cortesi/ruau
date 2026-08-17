use std::{
    any::{Any, TypeId},
    cell::{RefCell, RefMut},
    collections::HashMap,
    ops::{Deref, DerefMut},
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
pub struct ContextSlot<'entry> {
    value: RefCell<&'entry mut dyn Any>,
}

impl<'entry> ContextSlot<'entry> {
    pub(crate) fn new<T: Any>(value: &'entry mut T) -> Self {
        Self {
            value: RefCell::new(value),
        }
    }
}

#[allow(
    clippy::redundant_pub_crate,
    reason = "the private context module exports this trait to crate-wide VM entry code"
)]
pub(crate) trait ContextProvider {
    fn borrow_any_mut(&self) -> Option<RefMut<'_, dyn Any>>;
}

impl ContextProvider for ContextSlot<'_> {
    fn borrow_any_mut(&self) -> Option<RefMut<'_, dyn Any>> {
        let value = self.value.try_borrow_mut().ok()?;
        Some(RefMut::map(value, |value| &mut **value))
    }
}

/// App data and optional borrowed context visible to one VM entry.
#[derive(Clone, Copy)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "the private context module exports this carrier to crate-wide dispatch code"
)]
pub(crate) struct HostEntry<'entry> {
    app_data: &'entry RefCell<AppData>,
    payloads: &'entry crate::host_type::HostPayloadStore,
    context: Option<&'entry dyn ContextProvider>,
}

impl<'entry> HostEntry<'entry> {
    pub(crate) fn new(
        app_data: &'entry RefCell<AppData>,
        payloads: &'entry crate::host_type::HostPayloadStore,
    ) -> Self {
        Self {
            app_data,
            payloads,
            context: None,
        }
    }

    pub(crate) fn with_context(
        app_data: &'entry RefCell<AppData>,
        payloads: &'entry crate::host_type::HostPayloadStore,
        context: &'entry dyn ContextProvider,
    ) -> Self {
        Self {
            app_data,
            payloads,
            context: Some(context),
        }
    }

    pub(crate) fn app_data(self) -> &'entry RefCell<AppData> {
        self.app_data
    }

    pub(crate) fn context(self) -> Option<&'entry dyn ContextProvider> {
        self.context
    }

    pub(crate) fn payloads(self) -> &'entry crate::host_type::HostPayloadStore {
        self.payloads
    }
}

/// Mutable guard for the borrowed host context active on this VM entry.
pub struct ContextMut<'a, T: Any> {
    value: RefMut<'a, T>,
}

impl<'a, T: Any> ContextMut<'a, T> {
    pub(super) fn from_erased(value: RefMut<'a, dyn Any>) -> Option<Self> {
        RefMut::filter_map(value, |value| value.downcast_mut::<T>())
            .ok()
            .map(|value| Self { value })
    }
}

impl<T: Any> Deref for ContextMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T: Any> DerefMut for ContextMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}
