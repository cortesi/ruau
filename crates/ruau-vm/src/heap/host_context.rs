use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
};

use crate::scope::{AppData, ContextSlot};

/// Raw pointer to the VM app-data cell visible during scoped host calls.
#[derive(Clone, Copy)]
pub struct HostAppDataPtr(pub(super) NonNull<RefCell<AppData>>);

// The pointer is installed only while a VM entry owns `&mut Vm`; moving the VM
// between lane tasks remains safe because the pointee is VM-owned app data and
// no concurrent access is possible under that unique borrow.
unsafe impl Send for HostAppDataPtr {}

impl HostAppDataPtr {
    pub(crate) unsafe fn as_ref<'a>(self) -> &'a RefCell<AppData> {
        unsafe { self.0.as_ref() }
    }
}

/// Raw pointer to the borrowed host context visible during scoped host calls.
#[derive(Clone, Copy)]
pub struct HostContextPtr(pub(super) NonNull<ContextSlot>);

// The pointer is installed only while a VM entry owns `&mut Vm` and the
// borrowed context; no concurrent access is possible under that unique borrow.
unsafe impl Send for HostContextPtr {}

impl HostContextPtr {
    pub(crate) unsafe fn as_ref<'a>(self) -> &'a ContextSlot {
        unsafe { self.0.as_ref() }
    }
}

/// Restores the previous scoped-host app-data pointer when a VM segment exits.
pub struct HostAppDataGuard {
    pub(super) slot: NonNull<Cell<Option<HostAppDataPtr>>>,
    pub(super) previous: Option<HostAppDataPtr>,
}

impl Drop for HostAppDataGuard {
    fn drop(&mut self) {
        // SAFETY: `slot` points at the heap's separately-boxed app-data cell.
        // The guard is created from a live `&mut Heap` and dropped before the
        // active dispatch segment returns, so the heap — and the box it owns —
        // outlive the guard, and the boxed cell is never moved while the
        // segment runs. The write is a `Cell` store, so no reference into the
        // `Heap` allocation itself is formed.
        unsafe { self.slot.as_ref() }.set(self.previous);
    }
}

/// Restores the previous scoped-host context pointer when a VM segment exits.
pub struct HostContextGuard {
    pub(super) slot: NonNull<Cell<Option<HostContextPtr>>>,
    pub(super) previous: Option<HostContextPtr>,
}

impl Drop for HostContextGuard {
    fn drop(&mut self) {
        // SAFETY: `slot` points at the heap's separately-boxed context cell.
        unsafe { self.slot.as_ref() }.set(self.previous);
    }
}
