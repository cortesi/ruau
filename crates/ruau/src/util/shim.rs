//! Generic RAII helpers for shim-allocated FFI resources.
//!
//! A shim-allocated resource is a value owned by C code that must be freed through a
//! fixed entrypoint. The pair of [`FfiResource`] and [`RawGuard`] standardises this lifecycle
//! across the high-level crate.

/// A shim-allocated FFI resource that is released through a fixed entrypoint.
///
/// Each impl pairs a concrete C type with the matching free function. The trait is `Copy`
/// because the underlying values are typically plain C structs that own raw pointers.
pub(crate) trait FfiResource: Copy {
    /// Releases the resource through its native free function.
    ///
    /// # Safety
    ///
    /// The value must originate from the matching shim allocator and must not have been
    /// released already.
    unsafe fn release(self);
}

/// RAII guard that releases a shim-allocated FFI resource on scope exit.
pub(crate) struct RawGuard<T: FfiResource> {
    /// Raw resource allocated by the shim.
    raw: T,
}

impl<T: FfiResource> RawGuard<T> {
    /// Creates a guard for a shim-allocated resource.
    pub(crate) fn new(raw: T) -> Self {
        Self { raw }
    }

    /// Returns a shared reference to the underlying resource.
    pub(crate) fn as_ref(&self) -> &T {
        &self.raw
    }
}

impl<T: FfiResource> Drop for RawGuard<T> {
    fn drop(&mut self) {
        // SAFETY: `raw` originated from the shim and must be released exactly once. The trait
        // contract guarantees the value is fresh and unfreed when the guard is created.
        unsafe { self.raw.release() };
    }
}
