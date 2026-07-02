use std::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

use ruau_vm_api::{OwnedValue, RawGc, RawValue, RegistryRef, marker};

use super::RuntimeError;
use crate::heap::Heap;

/// A persistent, registry-rooted handle to a heap value. Unlike a scope-borrowed
/// handle it carries no brand, so it is valid *across* scope steps and awaits;
/// re-acquire the live value inside a scope (a future `scope.fetch_table`) or pass it to
/// a protected run to invoke. `T` is a zero-size [`marker`](ruau_vm_api::marker)
/// kind (`Str`, `Table`, …), so a `Stashed<ruau_vm_api::marker::Table>` cannot be
/// confused with a `Stashed<ruau_vm_api::marker::Str>`.
///
/// It is rooted by an underlying [`RegistryRef`] pin, so the value survives a
/// collection while the `Stashed` is live. Clones **share one pin**; dropping the
/// **last** clone enqueues the pin for release on the owning VM, which unpins it at
/// the start of its next step (a dropping thread needs no heap access). If the VM is
/// gone the pin leaks by contract.
///
/// Converting a `Stashed` into an [`OwnedValue`] transfers that shared pin to the
/// host-return boundary. After the transfer, any remaining clones are stale
/// handles: fetching through them fails closed instead of releasing or extending
/// the pin.
pub struct Stashed<T> {
    guard: std::sync::Arc<ReleaseGuard>,
    _marker: PhantomData<fn() -> T>,
}

/// The release-on-last-drop guard shared by a `Stashed`'s clones. When the final
/// `Arc` drops, this enqueues the pin for the owning VM to unpin on its next step —
/// so the pin is released exactly once, only after every clone is gone.
struct ReleaseGuard {
    reference: RegistryRef,
    release: std::sync::mpsc::Sender<RegistryRef>,
    transferred: AtomicBool,
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if self.transferred.load(Ordering::Acquire) {
            return;
        }
        // Nonblocking and panic-free: an `Err` means the VM's receiver is gone, in
        // which case the pin leaks by contract (the whole heap is being torn down).
        if self.release.send(self.reference.clone()).is_err() {}
    }
}

impl<T> Stashed<T> {
    /// Wraps a registry pin as a typed persistent handle, carrying the lane's
    /// release sender so the last clone can enqueue the pin on drop. The engine
    /// mints the pin; the marker `T` records the kind the slot holds.
    #[must_use]
    pub(crate) fn new(
        reference: RegistryRef,
        release: std::sync::mpsc::Sender<RegistryRef>,
    ) -> Self {
        Self {
            guard: std::sync::Arc::new(ReleaseGuard {
                reference,
                release,
                transferred: AtomicBool::new(false),
            }),
            _marker: PhantomData,
        }
    }

    /// The underlying registry pin.
    #[must_use]
    pub(crate) fn reference(&self) -> &RegistryRef {
        &self.guard.reference
    }

    /// Transfers this stash's registry pin into an owned host-return value.
    ///
    /// The returned [`OwnedValue::Pinned`] becomes responsible for releasing the
    /// pin when the engine materializes or discards the host return. The
    /// `Stashed` drop guard is disabled for every clone that shares this pin, so
    /// a clone retained by the host becomes a stale handle rather than
    /// double-releasing.
    #[must_use]
    pub fn into_owned_value(self) -> OwnedValue {
        self.guard.transferred.store(true, Ordering::Release);
        OwnedValue::Pinned(self.guard.reference.clone())
    }
}

impl<T> Clone for Stashed<T> {
    fn clone(&self) -> Self {
        Self {
            guard: self.guard.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for Stashed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stashed")
            .field("reference", &self.guard.reference)
            .finish()
    }
}

/// A VM-lifetime handle for a rooted interned string key.
///
/// Build one with [`Scope::intern_key`](crate::Scope::intern_key) and reuse it
/// with [`Table::get_keyed`](crate::Table::get_keyed) /
/// [`Table::set_keyed`](crate::Table::set_keyed) when repeatedly updating a
/// retained table. The handle keeps the interned key string registry-rooted, so
/// a collection cannot reclaim it between scope steps.
#[derive(Clone)]
pub struct KeyHandle {
    raw: RawGc<marker::Str>,
    key: Stashed<marker::Str>,
}

impl KeyHandle {
    pub(super) fn new(raw: RawGc<marker::Str>, key: Stashed<marker::Str>) -> Self {
        Self { raw, key }
    }

    pub(super) fn raw_value(&self, heap: &Heap) -> Result<RawValue, RuntimeError> {
        if self.raw.heap() == heap.id && heap.string(self.raw).is_some() {
            return Ok(RawValue::String(self.raw));
        }
        match heap.pinned_value(self.key.reference()) {
            Ok(RawValue::String(raw)) => Ok(RawValue::String(raw)),
            Ok(_) => Err(RuntimeError::runtime("key handle does not name a string")),
            Err(error) => Err(RuntimeError::runtime(format!(
                "key handle no longer resolves: {error}"
            ))),
        }
    }
}

impl std::fmt::Debug for KeyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _raw = self.raw;
        let _key = &self.key;
        f.write_str("KeyHandle { .. }")
    }
}

impl PartialEq for KeyHandle {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Eq for KeyHandle {}
