//! The borrowed lane handle (`Scope`) and its supporting types.
//!
//! A `Scope` is the embedding API's center: within one lane step the host borrows
//! it to build values, call Luau, and read state, and **no handle escapes the
//! step**. That guarantee is type-level, enforced by two cooperating pieces:
//!
//! - The lane step is a higher-ranked closure, `for<'s> FnOnce(&Scope<'s>) -> R`,
//!   whose return type `R` cannot name the scope brand `'s` — so a step cannot
//!   return a `'s`-borrowed handle.
//! - `R` must additionally be [`IntoStash`], a sealed allow-list of types that are
//!   safe to hand back out of a step: owned scalars, owned bytes, [`OwnedValue`],
//!   and the persistent [`Stashed`] handle. It is deliberately *not* implemented
//!   for the raw, unbranded handles ([`RawValue`]/`RawGc`), which are `'static`
//!   and so would slip past the brand alone.
//!
//! The lane step itself is [`Vm::step`](crate::Vm::step). The rich value
//! operations on the borrowed handles (`Table::get`/`set`, string/buffer
//! construction, the conversion traits) land with the value model; this module
//! establishes the borrow contract and a minimal handle the proof rejects on
//! escape.

use std::{
    any::{Any, TypeId},
    cell::{Ref, RefCell, RefMut},
    collections::{HashMap, HashSet},
    hash::{BuildHasher, Hash},
    marker::PhantomData,
};

#[cfg(any())]
use ruau_vm_api::RuntimeErrorKind;
use ruau_vm_api::{Gc, GcRawExt, HeapId, OwnedValue, RawGc, RawValue, RegistryRef, marker};

use crate::{
    MarshaledValue, ValueMarshalLimits, ValueVisitor, call,
    debug::SourceLocation,
    heap::Heap,
    load::LoadedModule,
    object::LuaBuffer,
    state::Thread,
    table::{LuaTable, key_rejection},
    vmutils,
};

mod context;
mod error;
mod stash;

pub use context::{AppData, ContextMut, ContextSlot};
pub use error::{RuntimeError, ScriptError};
pub use stash::{KeyHandle, Stashed};

/// Reserved lightuserdata handle used by `Scope::json_null`.
pub const JSON_NULL_LIGHTUSERDATA_HANDLE: u32 = 0x4f58_4a4e; // "OXJN"
/// Reserved lightuserdata handle used by JSON array marker metatables.
pub const JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE: u32 = 0x4f58_4a41; // "OXJA"
/// Reserved lightuserdata tag for Ruau-owned JSON bridge sentinels.
pub const JSON_BRIDGE_LIGHTUSERDATA_TAG: u8 = 0x4f; // "O"

mod sealed {
    /// Sealed so the [`IntoStash`](super::IntoStash) allow-list cannot be widened
    /// from outside this crate — in particular a downstream crate cannot teach a
    /// raw handle to satisfy it.
    #[allow(unnameable_types)] // Unnameability is the sealed-trait pattern's point.
    pub trait Sealed {}
}

/// The allow-list of types a [`Scope`] step may return. It is the type-level gate
/// that keeps a heap handle from escaping a step: a step's return type must be
/// `IntoStash`, and this trait is implemented for owned scalars, owned bytes,
/// [`OwnedValue`], and [`Stashed`] — but **never** for a scope-borrowed handle
/// (`ScopedValue<'s>`/…, which the higher-ranked closure already forbids) **nor** for the
/// raw, unbranded handles ([`RawValue`](ruau_vm_api::RawValue)/`RawGc`), which are
/// `'static` and would otherwise slip past the brand.
///
/// It is a sealed marker trait: it carries no conversion, only the permission to
/// leave a step. The only `IntoStash` types that can carry a heap reference out are
/// [`Stashed`] and [`OwnedValue::Pinned`], both of which are registry-*rooted* and
/// forge-resistant when the engine materializes them — they are the intentional
/// owned-handle channels, not a hole in the brand.
pub trait IntoStash: sealed::Sealed {}

macro_rules! impl_into_stash {
    ($($t:ty),* $(,)?) => {
        $(
            impl sealed::Sealed for $t {}
            impl IntoStash for $t {}
        )*
    };
}

// Owned scalars and owned byte/value payloads are always safe to return.
impl_into_stash!(
    (),
    bool,
    i64,
    f64,
    String,
    Vec<u8>,
    OwnedValue,
    MarshaledValue
);

// The debug-metadata values are plain owned data (no heap reference), so a step
// can return them directly.
impl_into_stash!(SourceLocation, FunctionInfo, FunctionId);

impl<T> sealed::Sealed for Stashed<T> {}
impl<T> IntoStash for Stashed<T> {}

impl sealed::Sealed for KeyHandle {}
impl IntoStash for KeyHandle {}

// `Option` and `Result`-of-stashable compose (the nil-or-error idiom); a tuple of
// stashables is a multi-return.
impl<T: IntoStash> sealed::Sealed for Option<T> {}
impl<T: IntoStash> IntoStash for Option<T> {}

impl<T: IntoStash, E: IntoStash> sealed::Sealed for Result<T, E> {}
impl<T: IntoStash, E: IntoStash> IntoStash for Result<T, E> {}

macro_rules! impl_into_stash_tuple {
    ($($name:ident),+) => {
        impl<$($name: IntoStash),+> sealed::Sealed for ($($name,)+) {}
        impl<$($name: IntoStash),+> IntoStash for ($($name,)+) {}
    };
}

impl_into_stash_tuple!(A);
impl_into_stash_tuple!(A, B);
impl_into_stash_tuple!(A, B, C);
impl_into_stash_tuple!(A, B, C, D);

macro_rules! borrowed_handle {
    ($name:ident, $marker:ty, $doc:literal) => {
        #[doc = $doc]
        ///
        /// The handle is branded by the scope step `'s` so it cannot outlive the
        /// step. Its debug form is deliberately opaque: raw handle parts are a
        /// capability-bearing implementation detail.
        #[derive(Clone, Copy)]
        pub struct $name<'s> {
            handle: Gc<'s, $marker>,
        }

        impl<'s> $name<'s> {
            #[must_use]
            pub(crate) fn from_raw(raw: RawGc<$marker>) -> Self {
                Self {
                    handle: Gc::from_raw(raw),
                }
            }
        }

        impl std::fmt::Debug for $name<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let _handle = &self.handle;
                f.write_str(concat!(stringify!($name), " { .. }"))
            }
        }
    };
}

borrowed_handle!(
    Str,
    marker::Str,
    "A scope-borrowed handle to an interned string."
);
borrowed_handle!(
    Table,
    marker::Table,
    "A scope-borrowed handle to a heap table."
);
borrowed_handle!(
    Function,
    marker::Closure,
    "A scope-borrowed handle to a Luau closure or host function."
);
borrowed_handle!(
    Userdata,
    marker::Userdata,
    "A scope-borrowed handle to host userdata."
);
borrowed_handle!(
    ThreadHandle,
    marker::Thread,
    "A scope-borrowed handle to a Luau coroutine thread."
);
borrowed_handle!(
    Buffer,
    marker::Buffer,
    "A scope-borrowed handle to a byte buffer."
);

impl Str<'_> {
    #[must_use]
    pub(crate) fn raw(self) -> RawGc<marker::Str> {
        self.handle.raw()
    }
}

impl Table<'_> {
    #[must_use]
    pub(crate) fn raw(self) -> RawGc<marker::Table> {
        self.handle.raw()
    }
}

impl<'s> Table<'s> {
    /// Reads `key` from this table, converting the result to `V`.
    ///
    /// This is a raw table lookup: it does not invoke `__index`; absent keys read
    /// as `nil`.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle no longer resolves, the key cannot
    /// be materialized, or the result cannot be converted to `V`.
    pub fn get<K, V>(self, scope: &Scope<'s>, key: K) -> Result<V, RuntimeError>
    where
        K: IntoLua<'s>,
        V: FromLua<'s>,
    {
        let key = key.into_lua(scope)?.into_raw();
        let raw = {
            let heap = scope.heap.borrow();
            heap.table(self.raw())
                .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?
                .get(key)
        };
        V::from_lua(ScopedValue::from_raw(raw), scope)
    }

    /// Reads an interned string key from this table, converting the result to
    /// `V`.
    ///
    /// This is the retained-table companion to [`Table::get`]: build a
    /// [`KeyHandle`] once with [`Scope::intern_key`] and reuse it across scope
    /// steps. The lookup is raw and does not invoke `__index`.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle or key handle no longer
    /// resolves, or the result cannot be converted to `V`.
    pub fn get_keyed<V>(self, scope: &Scope<'s>, key: &KeyHandle) -> Result<V, RuntimeError>
    where
        V: FromLua<'s>,
    {
        let raw = {
            let heap = scope.heap.borrow();
            let key = key.raw_value(&heap)?;
            heap.table(self.raw())
                .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?
                .get(key)
        };
        V::from_lua(ScopedValue::from_raw(raw), scope)
    }

    /// Writes `key = value` into this table.
    ///
    /// This is a raw table write: it does not invoke `__newindex`.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle no longer resolves, the key or value
    /// cannot be materialized, or the key is not a valid Lua table key.
    pub fn set<K, V>(self, scope: &Scope<'s>, key: K, value: V) -> Result<(), RuntimeError>
    where
        K: IntoLua<'s>,
        V: IntoLua<'s>,
    {
        let key = key.into_lua(scope)?.into_raw();
        let value = value.into_lua(scope)?.into_raw();
        let mut heap = scope.heap.borrow_mut();
        let table = heap
            .table_mut(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        if table.set(key, value) {
            Ok(())
        } else {
            let message =
                key_rejection(key).map_or("table key is invalid", |reason| reason.message());
            Err(RuntimeError::runtime(message))
        }
    }

    /// Writes `value` to the positive integer sequence index.
    ///
    /// The key is materialized as Luau's ordinary numeric array key, so writing
    /// exactly at `len + 1` grows the array border and can absorb contiguous
    /// successors previously written out of order.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle no longer resolves, the value
    /// cannot be materialized, or the index is not representable as an exact Lua
    /// array key.
    pub fn set_index<V>(self, scope: &Scope<'s>, index: u64, value: V) -> Result<(), RuntimeError>
    where
        V: IntoLua<'s>,
    {
        let key = sequence_key(index)?;
        let value = value.into_lua(scope)?.into_raw();
        let mut heap = scope.heap.borrow_mut();
        let table = heap
            .table_mut(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        if table.set(key, value) {
            Ok(())
        } else {
            Err(RuntimeError::runtime("table sequence index is invalid"))
        }
    }

    /// Appends one value after the current table border and returns its one-based
    /// index.
    ///
    /// This is host-side sequence construction; it does not invoke `__newindex`.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if reading the current border or writing the next
    /// slot fails.
    pub fn push<V>(self, scope: &Scope<'s>, value: V) -> Result<u64, RuntimeError>
    where
        V: IntoLua<'s>,
    {
        let index = self
            .len(scope)?
            .checked_add(1)
            .ok_or_else(|| RuntimeError::runtime("table sequence is too long"))?;
        self.set_index(scope, index, value)?;
        Ok(index)
    }

    /// Writes `key = value` through a rooted interned string key.
    ///
    /// This is the retained-table companion to [`Table::set`]: build a
    /// [`KeyHandle`] once with [`Scope::intern_key`] and reuse it across scope
    /// steps. The write is raw and does not invoke `__newindex`; passing `()`
    /// clears the field by writing `nil`.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle or key handle no longer
    /// resolves, or the value cannot be materialized.
    pub fn set_keyed<V>(
        self,
        scope: &Scope<'s>,
        key: &KeyHandle,
        value: V,
    ) -> Result<(), RuntimeError>
    where
        V: IntoLua<'s>,
    {
        let value = value.into_lua(scope)?.into_raw();
        let mut heap = scope.heap.borrow_mut();
        let key = key.raw_value(&heap)?;
        let table = heap
            .table_mut(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        if table.set(key, value) {
            Ok(())
        } else {
            Err(RuntimeError::runtime("key handle is not a valid table key"))
        }
    }

    /// Clears every entry whose key is not one of the supplied interned string
    /// handles.
    ///
    /// This is the retained-table cleanup companion to [`Table::set_keyed`].
    /// It resolves the rooted keys once, scans the raw table entries without
    /// materializing string contents, and writes `nil` through the same
    /// footprint-recharging table path as ordinary host writes.
    ///
    /// When `clear_non_string` is false, non-string keys are preserved even
    /// though they cannot appear in `keep`.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if this table or any key handle no longer
    /// resolves, or if the cleanup snapshot cannot be allocated.
    pub fn clear_except_keyed<'a>(
        self,
        scope: &Scope<'s>,
        keep: impl IntoIterator<Item = &'a KeyHandle>,
        clear_non_string: bool,
    ) -> Result<(), RuntimeError> {
        let mut heap = scope.heap.borrow_mut();
        let keep = keep
            .into_iter()
            .map(|key| key.raw_value(&heap))
            .collect::<Result<Vec<_>, _>>()?;
        let raw = self.raw();
        let mut to_clear = Vec::new();
        {
            let table = heap
                .table(raw)
                .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
            let mut live_entries = 0;
            table.for_each_entry(|_, _| live_entries += 1);
            to_clear
                .try_reserve(live_entries)
                .map_err(|_| RuntimeError::memory("out of memory snapshotting table keys"))?;
            table.for_each_entry(|key, _| {
                if keep.contains(&key) || (!clear_non_string && !matches!(key, RawValue::String(_)))
                {
                    return;
                }
                to_clear.push(key);
            });
        }
        let table = heap
            .table_mut(raw)
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        for key in to_clear {
            if !table.set(key, RawValue::Nil) {
                let message =
                    key_rejection(key).map_or("table key is invalid", |reason| reason.message());
                return Err(RuntimeError::runtime(message));
            }
        }
        Ok(())
    }

    /// Clears string-keyed entries that appeared in an earlier schema write but
    /// are absent from the current write.
    ///
    /// Unlike [`Table::clear_except_keyed`], this does not scan the table. It is
    /// intended for host-side retained schemas that already know the exact set
    /// of string keys they produced previously and now need to erase only their
    /// own stale keys.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if this table or any key handle no longer
    /// resolves.
    pub fn clear_stale_keyed<'a, 'b>(
        self,
        scope: &Scope<'s>,
        previous: impl IntoIterator<Item = &'a KeyHandle>,
        current: impl IntoIterator<Item = &'b KeyHandle>,
    ) -> Result<(), RuntimeError> {
        let mut heap = scope.heap.borrow_mut();
        let previous = previous
            .into_iter()
            .map(|key| key.raw_value(&heap))
            .collect::<Result<Vec<_>, _>>()?;
        let current = current
            .into_iter()
            .map(|key| key.raw_value(&heap))
            .collect::<Result<Vec<_>, _>>()?;
        let table = heap
            .table_mut(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        for key in previous {
            if current.contains(&key) {
                continue;
            }
            if !table.set(key, RawValue::Nil) {
                let message =
                    key_rejection(key).map_or("table key is invalid", |reason| reason.message());
                return Err(RuntimeError::runtime(message));
            }
        }
        Ok(())
    }

    /// Returns whether this table is read-only to Luau code.
    ///
    /// Host-side [`Table::set`] and [`Table::set_keyed`] remain trusted raw
    /// writes and can update a frozen table.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle no longer resolves.
    pub fn is_frozen(self, scope: &Scope<'s>) -> Result<bool, RuntimeError> {
        let heap = scope.heap.borrow();
        let table = heap
            .table(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        Ok(table.readonly)
    }

    /// Marks this table read-only to Luau code.
    ///
    /// This is the host-side counterpart to `table.freeze`, but it is
    /// idempotent and bypasses script-facing metatable protection because the
    /// caller already holds trusted host access.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle no longer resolves.
    pub fn freeze(self, scope: &Scope<'s>) -> Result<(), RuntimeError> {
        let mut heap = scope.heap.borrow_mut();
        let table = heap
            .table_mut(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        table.readonly = true;
        Ok(())
    }

    /// Recursively marks this table and table-valued keys/entries read-only to
    /// Luau code.
    ///
    /// The traversal follows ordinary table entries, including table keys. It
    /// intentionally does not follow metatables; freezing behavior metadata is a
    /// separate host decision.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if any table handle in the reachable graph no
    /// longer resolves.
    pub fn freeze_deep(self, scope: &Scope<'s>) -> Result<(), RuntimeError> {
        let mut heap = scope.heap.borrow_mut();
        let mut stack = vec![self.raw()];
        let mut seen = HashSet::new();
        while let Some(raw) = stack.pop() {
            if !seen.insert((raw.heap(), raw.index(), raw.generation())) {
                continue;
            }
            let table = heap
                .table_mut(raw)
                .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
            table.readonly = true;
            let mut children = Vec::new();
            table.for_each_entry(|key, value| {
                if let RawValue::Table(child) = key {
                    children.push(child);
                }
                if let RawValue::Table(child) = value {
                    children.push(child);
                }
            });
            stack.extend(children);
        }
        Ok(())
    }

    /// Returns this table's raw metatable, if any.
    ///
    /// This is host-side table inspection: it does not honor `__metatable`
    /// protection. Trusted host modules use it to preserve their own table
    /// markers; scripts still see the ordinary VM `getmetatable` behavior.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle no longer resolves.
    pub fn metatable(self, scope: &Scope<'s>) -> Result<Option<Self>, RuntimeError> {
        let heap = scope.heap.borrow();
        let table = heap
            .table(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        Ok(table.metatable().map(Table::from_raw))
    }

    /// Sets or clears this table's raw metatable.
    ///
    /// This is host-side table mutation: it does not invoke `setmetatable` and
    /// does not honor `__metatable` protection. Trusted host modules use it when
    /// constructing tables with host-owned marker metatables.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if either table handle no longer resolves.
    pub fn set_metatable(
        self,
        scope: &Scope<'s>,
        metatable: Option<Self>,
    ) -> Result<(), RuntimeError> {
        let raw_metatable = metatable.map(Table::raw);
        let mut heap = scope.heap.borrow_mut();
        if let Some(raw) = raw_metatable
            && heap.table(raw).is_none()
        {
            return Err(RuntimeError::runtime("metatable handle no longer resolves"));
        }
        let table = heap
            .table_mut(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        table.set_metatable(raw_metatable);
        Ok(())
    }

    /// Snapshots every live key/value pair in this table.
    ///
    /// This is raw table iteration: it does not invoke `__iter`, `__index`, or
    /// `pairs`. The returned pairs are scope-branded values, so they cannot
    /// escape the current [`Scope`] step.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle no longer resolves or the host-side
    /// snapshot allocation fails.
    pub fn pairs(
        self,
        scope: &Scope<'s>,
    ) -> Result<Vec<(ScopedValue<'s>, ScopedValue<'s>)>, RuntimeError> {
        let heap = scope.heap.borrow();
        let table = heap
            .table(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        let mut pairs = Vec::new();
        let mut live_entries = 0;
        table.for_each_entry(|_, _| live_entries += 1);
        pairs
            .try_reserve(live_entries)
            .map_err(|_| RuntimeError::memory("out of memory snapshotting table pairs"))?;
        table.for_each_entry(|key, value| {
            pairs.push((ScopedValue::from_raw(key), ScopedValue::from_raw(value)));
        });
        Ok(pairs)
    }

    /// Counts live key/value pairs without snapshotting them.
    pub(crate) fn pair_count(self, scope: &Scope<'s>) -> Result<usize, RuntimeError> {
        let heap = scope.heap.borrow();
        let table = heap
            .table(self.raw())
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))?;
        let mut live_entries = 0;
        table.for_each_entry(|_, _| live_entries += 1);
        Ok(live_entries)
    }

    /// Returns the table length operator's current border.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle no longer resolves.
    pub fn len(self, scope: &Scope<'s>) -> Result<u64, RuntimeError> {
        scope
            .heap
            .borrow()
            .table(self.raw())
            .map(LuaTable::length)
            .ok_or_else(|| RuntimeError::runtime("table handle no longer resolves"))
    }

    /// Whether [`Table::len`] is zero.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the table handle no longer resolves.
    pub fn is_empty(self, scope: &Scope<'s>) -> Result<bool, RuntimeError> {
        self.len(scope).map(|len| len == 0)
    }
}

fn sequence_key(index: u64) -> Result<RawValue, RuntimeError> {
    const MAX_EXACT_LUA_INTEGER: u64 = 9_007_199_254_740_992;
    if index == 0 {
        return Err(RuntimeError::runtime(
            "table sequence index must be positive",
        ));
    }
    if index > MAX_EXACT_LUA_INTEGER {
        return Err(RuntimeError::runtime(
            "table sequence index exceeds Lua number precision",
        ));
    }
    Ok(RawValue::Number(index as f64))
}

impl<'s> Buffer<'s> {
    #[must_use]
    pub(crate) fn raw(self) -> RawGc<marker::Buffer> {
        self.handle.raw()
    }

    /// The buffer length in bytes.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the buffer handle no longer resolves.
    pub fn len(self, scope: &Scope<'s>) -> Result<usize, RuntimeError> {
        scope
            .heap
            .borrow()
            .buffer(self.raw())
            .map(LuaBuffer::len)
            .ok_or_else(|| RuntimeError::runtime("buffer handle no longer resolves"))
    }

    /// Whether [`Buffer::len`] is zero.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the buffer handle no longer resolves.
    pub fn is_empty(self, scope: &Scope<'s>) -> Result<bool, RuntimeError> {
        self.len(scope).map(|len| len == 0)
    }

    /// Copies the buffer bytes into an owned vector.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the buffer handle no longer resolves.
    pub fn to_vec(self, scope: &Scope<'s>) -> Result<Vec<u8>, RuntimeError> {
        scope.buffer_bytes(self)
    }

    /// Writes `bytes` into the buffer at byte `offset`.
    ///
    /// The buffer's size is fixed. Writes that would overflow `usize` or extend
    /// past the end of the buffer fail without mutating it.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the buffer handle no longer resolves or the write is
    /// out of bounds.
    pub fn write(
        self,
        scope: &Scope<'s>,
        offset: usize,
        bytes: impl AsRef<[u8]>,
    ) -> Result<(), RuntimeError> {
        let bytes = bytes.as_ref();
        let end = offset
            .checked_add(bytes.len())
            .ok_or_else(|| RuntimeError::runtime("buffer write is out of bounds"))?;

        let mut heap = scope.heap.borrow_mut();
        let buffer = heap
            .buffer_mut(self.raw())
            .ok_or_else(|| RuntimeError::runtime("buffer handle no longer resolves"))?;
        if end > buffer.len() {
            return Err(RuntimeError::runtime("buffer write is out of bounds"));
        }
        buffer.bytes_mut()[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    /// Fills the buffer with `byte`.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the buffer handle no longer resolves.
    pub fn fill(self, scope: &Scope<'s>, byte: u8) -> Result<(), RuntimeError> {
        let mut heap = scope.heap.borrow_mut();
        let buffer = heap
            .buffer_mut(self.raw())
            .ok_or_else(|| RuntimeError::runtime("buffer handle no longer resolves"))?;
        buffer.bytes_mut().fill(byte);
        Ok(())
    }
}

impl<'s> Userdata<'s> {
    #[must_use]
    pub(crate) fn raw(self) -> RawGc<marker::Userdata> {
        self.handle.raw()
    }

    /// Return whether this userdata carries the registered host type `T`.
    #[must_use]
    pub fn is<T: Send + 'static>(self, scope: &Scope<'s>) -> bool {
        scope.userdata_is::<T>(self)
    }

    /// Borrows the embedded `T` shared, RefCell-style: any number of shared
    /// borrows may nest, and the returned guard derefs to `&T` for the rest of
    /// this scope step.
    ///
    /// Borrow violations are catchable [`RuntimeError`]s, never panics, and do
    /// not poison the VM: an exclusive borrow already live on this instance (a
    /// mut method re-entered through Lua) or a typed mismatch (`T` is not this
    /// value's registered type) both fail cleanly.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the handle no longer resolves, the value is
    /// of a different host type, or an exclusive borrow is live.
    pub fn borrow<T: Send + 'static>(
        self,
        scope: &Scope<'s>,
    ) -> Result<UserdataRef<'s, T>, RuntimeError> {
        let cell = scope.userdata_cell::<T>(self, BorrowMode::Shared)?;
        Ok(UserdataRef {
            cell,
            _brand: PhantomData,
        })
    }

    /// Borrows the embedded `T` exclusively; the returned guard derefs to
    /// `&mut T`. Fails with a catchable [`RuntimeError`] while any other
    /// borrow of the same instance is live — see [`Userdata::borrow`].
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the handle no longer resolves, the value is
    /// of a different host type, or any borrow is live.
    pub fn borrow_mut<T: Send + 'static>(
        self,
        scope: &Scope<'s>,
    ) -> Result<UserdataRefMut<'s, T>, RuntimeError> {
        let cell = scope.userdata_cell::<T>(self, BorrowMode::Exclusive)?;
        Ok(UserdataRefMut {
            cell,
            _brand: PhantomData,
        })
    }
}

/// Which borrow flag [`Scope::userdata_cell`] acquires before handing out the
/// payload pointer.
#[derive(Clone, Copy, Eq, PartialEq)]
enum BorrowMode {
    Shared,
    Exclusive,
}

/// A scope-branded shared borrow of the `T` inside a host userdata, from
/// [`Userdata::borrow`]. Dropping it releases the borrow flag. The brand keeps
/// it inside the step that minted it, which is what makes the pointer sound:
/// no collection can reclaim the userdata while any scope is live.
pub struct UserdataRef<'s, T: 'static> {
    cell: std::ptr::NonNull<crate::host_type::HostCell<T>>,
    /// `&'s` variance plus `!Send`/`!Sync` (via `NonNull`) match the borrow.
    _brand: PhantomData<&'s T>,
}

impl<T: 'static> std::ops::Deref for UserdataRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the shared flag is held, so no exclusive alias exists; the
        // cell outlives the guard (see `Userdata::borrow`).
        unsafe { &*self.cell.as_ref().value_ptr() }
    }
}

impl<T: 'static> Drop for UserdataRef<'_, T> {
    fn drop(&mut self) {
        // SAFETY: the cell outlives the guard (see `Userdata::borrow`).
        unsafe { self.cell.as_ref() }.release_shared();
    }
}

/// A scope-branded exclusive borrow of the `T` inside a host userdata, from
/// [`Userdata::borrow_mut`]. Dropping it releases the borrow flag.
pub struct UserdataRefMut<'s, T: 'static> {
    cell: std::ptr::NonNull<crate::host_type::HostCell<T>>,
    _brand: PhantomData<&'s mut T>,
}

impl<T: 'static> std::ops::Deref for UserdataRefMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the exclusive flag is held, so this guard is the only alias.
        unsafe { &*self.cell.as_ref().value_ptr() }
    }
}

impl<T: 'static> std::ops::DerefMut for UserdataRefMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the exclusive flag is held, so this guard is the only alias.
        unsafe { &mut *self.cell.as_ref().value_ptr() }
    }
}

impl<T: 'static> Drop for UserdataRefMut<'_, T> {
    fn drop(&mut self) {
        // SAFETY: the cell outlives the guard (see `Userdata::borrow_mut`).
        unsafe { self.cell.as_ref() }.release_exclusive();
    }
}

/// Debug metadata for a [`Function`], read from its prototype's debug data by
/// [`Function::info`].
///
/// Host and engine-builtin functions carry no Luau prototype data: they report
/// `host: true` with `chunk_name`/`line_defined` of `None` (a builtin still
/// reports its global name).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FunctionInfo {
    /// The chunk name the function's module was loaded under, in the same
    /// display form as error locations (`=name`/`@name` show `name`, a bare
    /// source shows `[string "…"]`). `None` for host and builtin functions.
    pub chunk_name: Option<String>,
    /// The 1-based source line where the function is defined. `None` for host
    /// and builtin functions.
    pub line_defined: Option<u32>,
    /// The function's debug name (a builtin's global name), if the compiler
    /// recorded one. Host functions report `None`.
    pub name: Option<String>,
    /// Whether this is a native (host or engine-builtin) function rather than a
    /// Luau closure.
    pub host: bool,
}

/// A stable identity token for one closure instance, from [`Function::id`].
///
/// Two `FunctionId`s compare equal exactly when they name the same closure
/// object: the id is stable for the closure's lifetime — across
/// [`Scope::stash_function`]/[`Scope::fetch_function`] round trips and GC
/// cycles while the closure is alive — and distinct closure instances (even two
/// closures of the same prototype) get distinct ids. Ids from different VMs
/// never compare equal.
///
/// After the closure is **collected**, its id may eventually be reused by a new
/// closure; treat a `FunctionId` as meaningful only while the host keeps the
/// closure alive (for example through a stash).
///
/// The token is opaque: it supports equality and hashing but exposes no parts,
/// and its `Debug` form is deliberately blank so a heap handle's raw identity
/// cannot leak through it.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct FunctionId {
    index: u32,
    generation: u32,
    heap: HeapId,
}

impl std::fmt::Debug for FunctionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FunctionId { .. }")
    }
}

impl<'s> Function<'s> {
    #[must_use]
    pub(crate) fn raw(self) -> RawGc<marker::Closure> {
        self.handle.raw()
    }

    /// Reads this function's debug metadata — definition site and name — from
    /// its prototype, the host-facing form of `debug.info`'s `s`/`l`/`n`
    /// options. Host and builtin functions report `host: true` with no
    /// definition site.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the function handle no longer resolves.
    pub fn info(self, scope: &Scope<'s>) -> Result<FunctionInfo, RuntimeError> {
        let heap = scope.heap.borrow();
        let proto = heap
            .closure(self.raw())
            .and_then(|closure| heap.proto(closure.proto))
            .ok_or_else(|| RuntimeError::runtime("function handle no longer resolves"))?;
        let chunk_name = proto
            .source
            .and_then(|source| heap.string(source))
            .map(|string| {
                String::from_utf8_lossy(&crate::debug::chunk_id(string.bytes())).into_owned()
            });
        let name = if let Some(native) = proto.native {
            Some(String::from_utf8_lossy(native.global_name()).into_owned())
        } else {
            proto
                .debug_name
                .and_then(|name| heap.string(name))
                .map(|name| String::from_utf8_lossy(name.bytes()).into_owned())
        };
        Ok(FunctionInfo {
            chunk_name,
            line_defined: (proto.line_defined > 0).then_some(proto.line_defined),
            name,
            host: proto.host.is_some() || proto.native.is_some(),
        })
    }

    /// A stable identity token for this closure instance (the host-facing form
    /// of `Function::to_pointer`). The identity is carried by the handle itself,
    /// so no heap access is needed; see [`FunctionId`] for the stability and
    /// reuse contract.
    #[must_use]
    pub fn id(self) -> FunctionId {
        let raw = self.raw();
        FunctionId {
            index: raw.index(),
            generation: raw.generation(),
            heap: raw.heap(),
        }
    }
}

/// A scope-borrowed Lua value. Heap-backed variants use branded handles, so a
/// value obtained inside one [`Scope`] step cannot be held across another step
/// or an async suspension unless the host explicitly roots it.
///
/// # Value representations
///
/// | Type | Role |
/// |------|------|
/// | [`ScopedValue`] | Scope-branded borrow for one [`Scope`] step |
/// | [`OwnedValue`](ruau_vm_api::OwnedValue) | Owned host return / async callback value |
/// | [`MarshaledValue`](crate::MarshaledValue) | VM-exit copy for durable storage serde |
/// | `ResultValue` | Tenant-facing report copy in the `ruau` runner |
/// | [`HostValue`](ruau_vm_api::HostValue) | Borrowed host callback argument |
/// | [`RawValue`](ruau_vm_api::RawValue) | Unbranded engine heap handle |
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum ScopedValue<'s> {
    /// `nil`.
    Nil,
    /// Boolean.
    Boolean(bool),
    /// IEEE-754 double.
    Number(f64),
    /// 64-bit integer.
    Integer(i64),
    /// Three-lane vector.
    Vector([f32; 3]),
    /// Opaque host token.
    LightUserdata {
        /// Host-defined payload.
        handle: u32,
        /// Host-defined tag.
        tag: u8,
    },
    /// Interned string.
    String(Str<'s>),
    /// Table.
    Table(Table<'s>),
    /// Closure or host function.
    Function(Function<'s>),
    /// Host userdata.
    Userdata(Userdata<'s>),
    /// Coroutine thread.
    Thread(ThreadHandle<'s>),
    /// Byte buffer.
    Buffer(Buffer<'s>),
}

impl<'s> ScopedValue<'s> {
    #[must_use]
    pub(crate) fn from_raw(raw: RawValue) -> Self {
        match raw {
            RawValue::Nil => Self::Nil,
            RawValue::Boolean(value) => Self::Boolean(value),
            RawValue::Number(value) => Self::Number(value),
            RawValue::Integer(value) => Self::Integer(value),
            RawValue::Vector(value) => Self::Vector(value),
            RawValue::LightUserdata { handle, tag } => Self::LightUserdata { handle, tag },
            RawValue::String(handle) => Self::String(Str::from_raw(handle)),
            RawValue::Table(handle) => Self::Table(Table::from_raw(handle)),
            RawValue::Function(handle) => Self::Function(Function::from_raw(handle)),
            RawValue::Userdata(handle) => Self::Userdata(Userdata::from_raw(handle)),
            RawValue::Thread(handle) => Self::Thread(ThreadHandle::from_raw(handle)),
            RawValue::Buffer(handle) => Self::Buffer(Buffer::from_raw(handle)),
        }
    }

    #[must_use]
    pub(crate) fn into_raw(self) -> RawValue {
        match self {
            Self::Nil => RawValue::Nil,
            Self::Boolean(value) => RawValue::Boolean(value),
            Self::Number(value) => RawValue::Number(value),
            Self::Integer(value) => RawValue::Integer(value),
            Self::Vector(value) => RawValue::Vector(value),
            Self::LightUserdata { handle, tag } => RawValue::LightUserdata { handle, tag },
            Self::String(value) => RawValue::String(value.raw()),
            Self::Table(value) => RawValue::Table(value.raw()),
            Self::Function(value) => RawValue::Function(value.handle.raw()),
            Self::Userdata(value) => RawValue::Userdata(value.handle.raw()),
            Self::Thread(value) => RawValue::Thread(value.handle.raw()),
            Self::Buffer(value) => RawValue::Buffer(value.raw()),
        }
    }

    /// Luau's ordinary type name for this value.
    #[must_use]
    pub fn type_name(self) -> &'static str {
        match self {
            Self::Nil => "nil",
            Self::Boolean(_) => "boolean",
            Self::Number(_) | Self::Integer(_) => "number",
            Self::Vector(_) => "vector",
            Self::LightUserdata { .. } | Self::Userdata(_) => "userdata",
            Self::String(_) => "string",
            Self::Table(_) => "table",
            Self::Function(_) => "function",
            Self::Thread(_) => "thread",
            Self::Buffer(_) => "buffer",
        }
    }

    /// Conservative display text for this value.
    ///
    /// Strings return their bytes lossily decoded as UTF-8. Scalar values use
    /// Luau's scalar spelling. Heap objects return their type name; this helper
    /// does not call `tostring` or run metamethods.
    #[must_use]
    pub fn display(self, scope: &Scope<'s>) -> String {
        match self {
            Self::Nil => "nil".to_owned(),
            Self::Boolean(true) => "true".to_owned(),
            Self::Boolean(false) => "false".to_owned(),
            Self::Number(value) => vmutils::number_to_string(value),
            Self::Integer(value) => value.to_string(),
            Self::Vector(value) => value
                .iter()
                .map(|component| vmutils::number_to_string(f64::from(*component)))
                .collect::<Vec<_>>()
                .join(", "),
            Self::String(value) => scope.string_bytes(value).map_or_else(
                |_| "<dangling string>".to_owned(),
                |bytes| String::from_utf8_lossy(&bytes).into_owned(),
            ),
            other => other.type_name().to_owned(),
        }
    }

    /// Converts this scope-borrowed value into an owned host-return value.
    ///
    /// Immediates and strings are copied directly. Heap-backed values that
    /// cannot be represented as plain owned data are registry-pinned as
    /// [`OwnedValue::Pinned`], so they can be materialized safely at a later VM
    /// boundary without a raw handle escaping the scope.
    ///
    /// # Errors
    /// [`RuntimeError`] if a string handle no longer resolves or the registry
    /// pin would exceed the VM's memory cap.
    pub fn to_owned_value(self, scope: &Scope<'s>) -> Result<OwnedValue, RuntimeError> {
        scope.owned_value(self)
    }
}

impl std::fmt::Debug for ScopedValue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nil => f.write_str("Nil"),
            Self::Boolean(value) => f.debug_tuple("Boolean").field(value).finish(),
            Self::Number(value) => f.debug_tuple("Number").field(value).finish(),
            Self::Integer(value) => f.debug_tuple("Integer").field(value).finish(),
            Self::Vector(value) => f.debug_tuple("Vector").field(value).finish(),
            Self::LightUserdata { .. } => f.write_str("LightUserdata { .. }"),
            Self::String(_) => f.write_str("String { .. }"),
            Self::Table(_) => f.write_str("Table { .. }"),
            Self::Function(_) => f.write_str("Function { .. }"),
            Self::Userdata(_) => f.write_str("Userdata { .. }"),
            Self::Thread(_) => f.write_str("Thread { .. }"),
            Self::Buffer(_) => f.write_str("Buffer { .. }"),
        }
    }
}

/// A vector of scope-borrowed Lua values.
#[derive(Clone, Debug, Default)]
pub struct MultiValue<'s> {
    values: Vec<ScopedValue<'s>>,
}

impl<'s> MultiValue<'s> {
    /// Builds an empty multi-value.
    #[must_use]
    pub fn new() -> Self {
        Self { values: Vec::new() }
    }

    /// Builds a multi-value from already-branded values.
    #[must_use]
    pub fn from_values(values: Vec<ScopedValue<'s>>) -> Self {
        Self { values }
    }

    /// Number of values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether there are no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Iterates over the values.
    pub fn iter(&self) -> impl Iterator<Item = ScopedValue<'s>> + '_ {
        self.values.iter().copied()
    }

    /// Consumes the wrapper and returns the value vector.
    #[must_use]
    pub fn into_vec(self) -> Vec<ScopedValue<'s>> {
        self.values
    }

    #[must_use]
    pub(crate) fn from_raw_values(values: Vec<RawValue>) -> Self {
        Self::from_values(values.into_iter().map(ScopedValue::from_raw).collect())
    }

    #[must_use]
    pub(crate) fn into_raw_vec(self) -> Vec<RawValue> {
        self.values.into_iter().map(ScopedValue::into_raw).collect()
    }
}

/// Converts a Rust value into a scope-borrowed Lua value.
pub trait IntoLua<'s>: Sized {
    /// Materializes `self` inside `scope`.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if materializing the value requires a failed heap
    /// allocation or the value is not representable in Lua.
    fn into_lua(self, scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError>;
}

/// Converts a scope-borrowed Lua value into a Rust value.
pub trait FromLua<'s>: Sized {
    /// Reads `value` using `scope` for heap-backed data.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the value has the wrong Lua type or cannot be decoded.
    fn from_lua(value: ScopedValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError>;
}

/// Converts Rust return values into Lua's multi-return shape.
pub trait IntoLuaMulti<'s>: Sized {
    /// Materializes `self` as zero or more Lua values.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if any element cannot be materialized.
    fn into_lua_multi(self, scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError>;
}

/// Converts Lua's multi-return shape into a Rust value.
pub trait FromLuaMulti<'s>: Sized {
    /// Reads `values` using `scope` for heap-backed data.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the arity or any element type is wrong.
    fn from_lua_multi(values: MultiValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError>;
}

/// Decoded arguments for a Luau method-style call (`receiver:method(...)`).
///
/// Luau passes the receiver as the first argument. This wrapper splits that
/// receiver from the remaining arguments and lets both sides use the ordinary
/// [`FromLua`] / [`FromLuaMulti`] conversions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MethodArgs<R, A = ()> {
    /// The receiver passed before the explicit arguments.
    pub receiver: R,
    /// Explicit arguments after the receiver.
    pub args: A,
}

impl<R, A> MethodArgs<R, A> {
    /// Builds method arguments from their parts.
    #[must_use]
    pub const fn new(receiver: R, args: A) -> Self {
        Self { receiver, args }
    }

    /// Consumes the wrapper into `(receiver, args)`.
    #[must_use]
    pub fn into_parts(self) -> (R, A) {
        (self.receiver, self.args)
    }
}

impl<'s, R, A> FromLuaMulti<'s> for MethodArgs<R, A>
where
    R: FromLua<'s>,
    A: FromLuaMulti<'s>,
{
    fn from_lua_multi(values: MultiValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        let mut values = values.into_vec().into_iter();
        let receiver = values
            .next()
            .ok_or_else(|| arity_error(1, 0))
            .and_then(|value| {
                R::from_lua(value, scope).map_err(|error| path_error("receiver", error))
            })?;
        let args = A::from_lua_multi(MultiValue::from_values(values.collect()), scope)
            .map_err(|error| path_error("method arguments", error))?;
        Ok(Self { receiver, args })
    }
}

fn conversion_error(expected: &'static str, got: ScopedValue<'_>) -> RuntimeError {
    RuntimeError::runtime(format!("expected {expected}, got {}", got.type_name()))
}

fn number_to_integer(value: f64) -> Result<i64, RuntimeError> {
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "fract()==0 and the range guard keep the cast exact"
    )]
    if value.fract() == 0.0 && (-TWO_POW_63..TWO_POW_63).contains(&value) {
        return Ok(value as i64);
    }
    if value.fract() == 0.0 {
        return Err(RuntimeError::runtime(format!(
            "number {value} is out of range for a 64-bit integer"
        )));
    }
    Err(RuntimeError::runtime(format!(
        "expected integer, got non-integral number {value}"
    )))
}

fn arity_error(expected: usize, got: usize) -> RuntimeError {
    RuntimeError::runtime(format!("expected {expected} Lua values, got {got}"))
}

fn path_error(path: impl std::fmt::Display, error: RuntimeError) -> RuntimeError {
    error.with_path(path)
}

/// Formats a runtime-compile failure as `chunk_name:line: message`, matching
/// `loadstring`'s location shape: the display form of the chunk name
/// (`chunk_id`) ahead of the compiler's `:line: text` payload. A diagnostic
/// without the location prefix (a compile-cap violation, a cancelled compile)
/// gets a plain `chunk_name: message` separator instead.
fn chunk_compile_error(chunk_name: &[u8], body: &[u8]) -> RuntimeError {
    let mut message = crate::debug::chunk_id(chunk_name);
    if !body.starts_with(b":") {
        message.extend_from_slice(b": ");
    }
    message.extend_from_slice(body);
    RuntimeError::runtime(String::from_utf8_lossy(&message))
}

/// Maps a structural load failure of a runtime-compiled chunk to a host-facing
/// error naming the chunk, preserving the out-of-memory category.
fn chunk_load_error(chunk_name: &[u8], error: &crate::load::LoadError) -> RuntimeError {
    let name = String::from_utf8_lossy(&crate::debug::chunk_id(chunk_name)).into_owned();
    let message = format!("{name}: {error}");
    match error {
        crate::load::LoadError::OutOfMemory => RuntimeError::memory(message),
        _ => RuntimeError::runtime(message),
    }
}

impl<'s> IntoLua<'s> for ScopedValue<'s> {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        Ok(self)
    }
}

impl<'s> FromLua<'s> for ScopedValue<'s> {
    fn from_lua(value: Self, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        Ok(value)
    }
}

impl<'s> IntoLua<'s> for () {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::Nil)
    }
}

impl<'s> FromLua<'s> for () {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Nil => Ok(()),
            other => Err(conversion_error("nil", other)),
        }
    }
}

impl<'s> IntoLua<'s> for bool {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::Boolean(self))
    }
}

impl<'s> FromLua<'s> for bool {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Boolean(value) => Ok(value),
            other => Err(conversion_error("boolean", other)),
        }
    }
}

impl<'s> IntoLua<'s> for i64 {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::Integer(self))
    }
}

impl<'s> FromLua<'s> for i64 {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Integer(value) => Ok(value),
            ScopedValue::Number(value) => number_to_integer(value),
            other => Err(conversion_error("integer", other)),
        }
    }
}

macro_rules! impl_signed_integer_lua {
    ($($t:ty),+ $(,)?) => {
        $(
            impl<'s> IntoLua<'s> for $t {
                fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
                    i64::try_from(self)
                        .map(ScopedValue::Integer)
                        .map_err(|_| RuntimeError::runtime(concat!("integer out of range for Lua: ", stringify!($t))))
                }
            }

            impl<'s> FromLua<'s> for $t {
                fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
                    match value {
                        ScopedValue::Integer(value) => <$t>::try_from(value)
                            .map_err(|_| RuntimeError::runtime(concat!("integer out of range for ", stringify!($t)))),
                        ScopedValue::Number(value) => <$t>::try_from(number_to_integer(value)?)
                            .map_err(|_| RuntimeError::runtime(concat!("integer out of range for ", stringify!($t)))),
                        other => Err(conversion_error("integer", other)),
                    }
                }
            }
        )+
    };
}

macro_rules! impl_unsigned_integer_lua {
    ($($t:ty),+ $(,)?) => {
        $(
            impl<'s> IntoLua<'s> for $t {
                fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
                    i64::try_from(self)
                        .map(ScopedValue::Integer)
                        .map_err(|_| RuntimeError::runtime(concat!("integer out of range for Lua: ", stringify!($t))))
                }
            }

            impl<'s> FromLua<'s> for $t {
                fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
                    match value {
                        ScopedValue::Integer(value) => <$t>::try_from(value)
                            .map_err(|_| RuntimeError::runtime(concat!("integer out of range for ", stringify!($t)))),
                        ScopedValue::Number(value) => <$t>::try_from(number_to_integer(value)?)
                            .map_err(|_| RuntimeError::runtime(concat!("integer out of range for ", stringify!($t)))),
                        other => Err(conversion_error("integer", other)),
                    }
                }
            }
        )+
    };
}

impl_signed_integer_lua!(i8, i16, i32, isize);
impl_unsigned_integer_lua!(u8, u16, u32, u64, usize);

impl<'s> IntoLua<'s> for f32 {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::Number(f64::from(self)))
    }
}

impl<'s> FromLua<'s> for f32 {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Number(value) => Ok(value as Self),
            ScopedValue::Integer(value) => Ok(value as Self),
            other => Err(conversion_error("number", other)),
        }
    }
}

impl<'s> IntoLua<'s> for f64 {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::Number(self))
    }
}

impl<'s> FromLua<'s> for f64 {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Number(value) => Ok(value),
            ScopedValue::Integer(value) => Ok(value as Self),
            other => Err(conversion_error("number", other)),
        }
    }
}

impl<'s> IntoLua<'s> for &str {
    fn into_lua(self, scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::String(scope.create_string(self.as_bytes())?))
    }
}

impl<'s> IntoLua<'s> for String {
    fn into_lua(self, scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        self.as_str().into_lua(scope)
    }
}

impl<'s> IntoLua<'s> for &[u8] {
    fn into_lua(self, scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::String(scope.create_string(self)?))
    }
}

impl<'s> FromLua<'s> for String {
    fn from_lua(value: ScopedValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        let ScopedValue::String(value) = value else {
            return Err(conversion_error("string", value));
        };
        Self::from_utf8(scope.string_bytes(value)?)
            .map_err(|_| RuntimeError::runtime("expected UTF-8 string"))
    }
}

impl<'s> IntoLua<'s> for Str<'s> {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::String(self))
    }
}

impl<'s> FromLua<'s> for Str<'s> {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::String(value) => Ok(value),
            other => Err(conversion_error("string", other)),
        }
    }
}

impl<'s> IntoLua<'s> for Table<'s> {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::Table(self))
    }
}

impl<'s> FromLua<'s> for Table<'s> {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Table(value) => Ok(value),
            other => Err(conversion_error("table", other)),
        }
    }
}

impl<'s> IntoLua<'s> for Function<'s> {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::Function(self))
    }
}

impl<'s> FromLua<'s> for Function<'s> {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Function(value) => Ok(value),
            other => Err(conversion_error("function", other)),
        }
    }
}

impl<'s> IntoLua<'s> for Buffer<'s> {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::Buffer(self))
    }
}

impl<'s> FromLua<'s> for Buffer<'s> {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Buffer(value) => Ok(value),
            other => Err(conversion_error("buffer", other)),
        }
    }
}

impl<'s> IntoLua<'s> for Userdata<'s> {
    fn into_lua(self, _scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        Ok(ScopedValue::Userdata(self))
    }
}

impl<'s> FromLua<'s> for Userdata<'s> {
    fn from_lua(value: ScopedValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Userdata(value) => Ok(value),
            other => Err(conversion_error("userdata", other)),
        }
    }
}

impl<'s, T> IntoLua<'s> for Option<T>
where
    T: IntoLua<'s>,
{
    fn into_lua(self, scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        match self {
            Some(value) => value.into_lua(scope),
            None => Ok(ScopedValue::Nil),
        }
    }
}

impl<'s, T> FromLua<'s> for Option<T>
where
    T: FromLua<'s>,
{
    fn from_lua(value: ScopedValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match value {
            ScopedValue::Nil => Ok(None),
            other => T::from_lua(other, scope).map(Some),
        }
    }
}

impl<'s, T> IntoLua<'s> for Vec<T>
where
    T: IntoLua<'s>,
{
    fn into_lua(self, scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        let mut array = Vec::new();
        array
            .try_reserve(self.len())
            .map_err(|_| RuntimeError::memory("out of memory materializing a Vec as a table"))?;
        for (index, value) in self.into_iter().enumerate() {
            let lua_value = value
                .into_lua(scope)
                .map_err(|error| path_error(format!("[{}]", index + 1), error))?;
            array.push(lua_value.into_raw());
        }
        let raw = scope
            .heap
            .borrow_mut()
            .alloc_table(LuaTable::with_array(array))
            .ok_or_else(|| RuntimeError::memory("out of memory materializing a Vec as a table"))?;
        Ok(ScopedValue::Table(Table::from_raw(raw)))
    }
}

impl<'s, T> FromLua<'s> for Vec<T>
where
    T: FromLua<'s>,
{
    fn from_lua(value: ScopedValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        let table = Table::from_lua(value, scope)?;
        let len = usize::try_from(table.len(scope)?)
            .map_err(|_| RuntimeError::runtime("table array length does not fit usize"))?;
        let mut out = Self::new();
        out.try_reserve(len)
            .map_err(|_| RuntimeError::memory("out of memory materializing a table as a Vec"))?;
        for index in 1..=len {
            let value = table
                .get(scope, index as f64)
                .map_err(|error| path_error(format!("[{index}]"), error))?;
            out.push(value);
        }
        Ok(out)
    }
}

impl<'s, K, V, S> IntoLua<'s> for HashMap<K, V, S>
where
    K: IntoLua<'s>,
    V: IntoLua<'s>,
    S: BuildHasher,
{
    fn into_lua(self, scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        let table = scope.create_table()?;
        for (index, (key, value)) in self.into_iter().enumerate() {
            let key = key
                .into_lua(scope)
                .map_err(|error| path_error(format!("map pair {} key", index + 1), error))?;
            let value = value
                .into_lua(scope)
                .map_err(|error| path_error(format!("map pair {} value", index + 1), error))?;
            table.set(scope, key, value)?;
        }
        Ok(ScopedValue::Table(table))
    }
}

impl<'s, K, V, S> FromLua<'s> for HashMap<K, V, S>
where
    K: FromLua<'s> + Eq + Hash,
    V: FromLua<'s>,
    S: BuildHasher + Default,
{
    fn from_lua(value: ScopedValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        let table = Table::from_lua(value, scope)?;
        let pairs = table.pairs(scope)?;
        let mut out = Self::with_capacity_and_hasher(pairs.len(), S::default());
        for (index, (key, value)) in pairs.into_iter().enumerate() {
            let key = K::from_lua(key, scope)
                .map_err(|error| path_error(format!("map pair {} key", index + 1), error))?;
            let value = V::from_lua(value, scope)
                .map_err(|error| path_error(format!("map pair {} value", index + 1), error))?;
            out.insert(key, value);
        }
        Ok(out)
    }
}

impl<'s, T> IntoLua<'s> for Result<T, RuntimeError>
where
    T: IntoLua<'s>,
{
    fn into_lua(self, scope: &Scope<'s>) -> Result<ScopedValue<'s>, RuntimeError> {
        self?.into_lua(scope)
    }
}

impl<'s> IntoLuaMulti<'s> for MultiValue<'s> {
    fn into_lua_multi(self, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        Ok(self)
    }
}

impl<'s> FromLuaMulti<'s> for MultiValue<'s> {
    fn from_lua_multi(values: Self, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        Ok(values)
    }
}

impl<'s> IntoLuaMulti<'s> for () {
    fn into_lua_multi(self, _scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError> {
        Ok(MultiValue::new())
    }
}

impl<'s> FromLuaMulti<'s> for () {
    fn from_lua_multi(values: MultiValue<'s>, _scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        if values.is_empty() {
            Ok(())
        } else {
            Err(arity_error(0, values.len()))
        }
    }
}

macro_rules! impl_single_lua_multi {
    ($($t:ty),+ $(,)?) => {
        $(
            impl<'s> IntoLuaMulti<'s> for $t {
                fn into_lua_multi(self, scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError> {
                    Ok(MultiValue::from_values(vec![self.into_lua(scope)?]))
                }
            }

            impl<'s> FromLuaMulti<'s> for $t {
                fn from_lua_multi(values: MultiValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
                    if values.len() != 1 {
                        return Err(arity_error(1, values.len()));
                    }
                    let mut values = values.into_vec().into_iter();
                    Self::from_lua(values.next().expect("arity checked"), scope)
                }
            }
        )+
    };
}

impl_single_lua_multi!(
    ScopedValue<'s>,
    bool,
    i8,
    i16,
    i32,
    i64,
    isize,
    u8,
    u16,
    u32,
    u64,
    usize,
    f32,
    f64,
    String,
    Str<'s>,
    Table<'s>,
    Function<'s>,
    Userdata<'s>,
    Buffer<'s>
);

impl<'s> IntoLuaMulti<'s> for &str {
    fn into_lua_multi(self, scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError> {
        Ok(MultiValue::from_values(vec![self.into_lua(scope)?]))
    }
}

impl<'s> IntoLuaMulti<'s> for &[u8] {
    fn into_lua_multi(self, scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError> {
        Ok(MultiValue::from_values(vec![self.into_lua(scope)?]))
    }
}

impl<'s, T> IntoLuaMulti<'s> for Vec<T>
where
    Self: IntoLua<'s>,
{
    fn into_lua_multi(self, scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError> {
        Ok(MultiValue::from_values(vec![self.into_lua(scope)?]))
    }
}

impl<'s, T> FromLuaMulti<'s> for Vec<T>
where
    Self: FromLua<'s>,
{
    fn from_lua_multi(values: MultiValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        if values.len() != 1 {
            return Err(arity_error(1, values.len()));
        }
        let mut values = values.into_vec().into_iter();
        Self::from_lua(values.next().expect("arity checked"), scope)
    }
}

impl<'s, K, V, S> IntoLuaMulti<'s> for HashMap<K, V, S>
where
    Self: IntoLua<'s>,
{
    fn into_lua_multi(self, scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError> {
        Ok(MultiValue::from_values(vec![self.into_lua(scope)?]))
    }
}

impl<'s, K, V, S> FromLuaMulti<'s> for HashMap<K, V, S>
where
    Self: FromLua<'s>,
{
    fn from_lua_multi(values: MultiValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        if values.len() != 1 {
            return Err(arity_error(1, values.len()));
        }
        let mut values = values.into_vec().into_iter();
        Self::from_lua(values.next().expect("arity checked"), scope)
    }
}

impl<'s, T> IntoLuaMulti<'s> for Option<T>
where
    T: IntoLua<'s>,
{
    fn into_lua_multi(self, scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError> {
        Ok(MultiValue::from_values(vec![self.into_lua(scope)?]))
    }
}

impl<'s, T> FromLuaMulti<'s> for Option<T>
where
    T: FromLua<'s>,
{
    fn from_lua_multi(values: MultiValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
        match values.len() {
            0 => Ok(None),
            1 => {
                let mut values = values.into_vec().into_iter();
                Self::from_lua(values.next().expect("arity checked"), scope)
            }
            count => Err(arity_error(1, count)),
        }
    }
}

impl<'s, T> IntoLuaMulti<'s> for Result<T, RuntimeError>
where
    T: IntoLuaMulti<'s>,
{
    fn into_lua_multi(self, scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError> {
        self?.into_lua_multi(scope)
    }
}

macro_rules! impl_lua_multi_tuple {
    ($(($name:ident, $var:ident)),+ $(,)?) => {
        impl<'s, $($name),+> IntoLuaMulti<'s> for ($($name,)+)
        where
            $($name: IntoLua<'s>,)+
        {
            fn into_lua_multi(self, scope: &Scope<'s>) -> Result<MultiValue<'s>, RuntimeError> {
                let ($($var,)+) = self;
                Ok(MultiValue::from_values(vec![$($var.into_lua(scope)?,)+]))
            }
        }

        impl<'s, $($name),+> FromLuaMulti<'s> for ($($name,)+)
        where
            $($name: FromLua<'s>,)+
        {
            fn from_lua_multi(values: MultiValue<'s>, scope: &Scope<'s>) -> Result<Self, RuntimeError> {
                const EXPECTED: usize = 0 $(+ { let _ = stringify!($name); 1 })+;
                if values.len() != EXPECTED {
                    return Err(arity_error(EXPECTED, values.len()));
                }
                let mut values = values.into_vec().into_iter();
                Ok(($($name::from_lua(values.next().expect("arity checked"), scope)?,)+))
            }
        }
    };
}

impl_lua_multi_tuple!((A, a));
impl_lua_multi_tuple!((A, a), (B, b));
impl_lua_multi_tuple!((A, a), (B, b), (C, c));
impl_lua_multi_tuple!((A, a), (B, b), (C, c), (D, d));

/// The borrowed lane handle: within one step the host borrows `&Scope` to build
/// values, persist them, and (later) call Luau and read state. It is
/// interior-mutable behind `&self` (the mlua model), so multiple handles compose
/// while the step holds the single mutable borrow of the heap. The `'s` brand is
/// invariant and generative: a handle minted in one step cannot be carried into
/// another, and no handle escapes the step (see [`Vm::step`](crate::Vm::step)).
pub struct Scope<'s> {
    heap: RefCell<&'s mut Heap>,
    /// The lane's main thread, taken out of the arena by `Vm::step` so a
    /// [`Scope::call`] has the disjoint `&mut Heap` + `&mut Thread` a nested Luau
    /// call needs. Only `call`/`global` touch it; value construction uses the heap.
    thread: RefCell<&'s mut Thread>,
    /// Host app data, behind its **own** cell so reads do not collide with the heap
    /// borrow this step holds (see [`AppData`]).
    app_data: &'s RefCell<AppData>,
    /// Borrowed host context lent for this VM entry, if one is active.
    context: Option<&'s ContextSlot>,
    /// Invariant in `'s` so the brand is generative — two scopes' lifetimes never
    /// unify, so a handle cannot cross between them.
    _brand: PhantomData<fn(&'s ()) -> &'s ()>,
    /// Whether this scope acquired the heap's public-scope guard and must release
    /// it on drop. Nested host-call scopes can share an outer guard.
    exit_scope_on_drop: bool,
}

impl<'s> Scope<'s> {
    /// Wraps the lane's heap, main thread, and app data for one step. The caller
    /// (`Vm::step`) has already set the re-entry guard and taken the main thread
    /// out; [`Scope`]'s `Drop` clears the guard (the caller puts the thread back),
    /// so both are released even if the step body panics.
    pub(crate) fn new(
        heap: &'s mut Heap,
        thread: &'s mut Thread,
        app_data: &'s RefCell<AppData>,
    ) -> Self {
        Self::with_scope_guard(heap, thread, app_data, true)
    }

    /// Wraps a host call in the scoped value model. `exit_scope_on_drop` is true
    /// when this call opened the heap's scope guard itself, and false when it is
    /// nested under an existing `Vm::step` / host-call scope.
    pub(crate) fn with_scope_guard(
        heap: &'s mut Heap,
        thread: &'s mut Thread,
        app_data: &'s RefCell<AppData>,
        exit_scope_on_drop: bool,
    ) -> Self {
        let context = heap.active_host_context_ptr().map(|context| {
            // SAFETY: the pointer is installed by a live VM entry guard and
            // restored before the borrowed context can go out of scope.
            unsafe { context.as_ref() }
        });
        Self {
            heap: RefCell::new(heap),
            thread: RefCell::new(thread),
            app_data,
            context,
            _brand: PhantomData,
            exit_scope_on_drop,
        }
    }

    pub(crate) fn with_active_host_app_data_guard(
        heap: &'s mut Heap,
        thread: &'s mut Thread,
    ) -> Option<Self> {
        let app_data = heap.active_host_app_data_ptr()?;
        let exit_scope_on_drop = heap.try_enter_scope();
        // SAFETY: the pointer is installed by a live `Vm` entry guard and
        // restored on drop. While host dispatch is running, the VM entry owns
        // the app-data cell and keeps it resident for this heap.
        let app_data = unsafe { app_data.as_ref() };
        Some(Self::with_scope_guard(
            heap,
            thread,
            app_data,
            exit_scope_on_drop,
        ))
    }

    /// Creates a fresh empty table, returning a scope-borrowed handle.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if the allocation would exceed the VM's memory cap.
    pub fn create_table(&self) -> Result<Table<'s>, RuntimeError> {
        let raw = self
            .heap
            .borrow_mut()
            .alloc_table(LuaTable::new())
            .ok_or_else(|| RuntimeError::memory("out of memory creating a table"))?;
        Ok(Table::from_raw(raw))
    }

    /// Returns the engine-owned JSON `null` sentinel.
    ///
    /// The ordinary serde bridge still maps unit/`None`/nil together. JSON
    /// fidelity helpers use this reserved lightuserdata value when a dynamic
    /// JSON document must distinguish `null` from absent table fields.
    #[must_use]
    pub fn json_null(&self) -> ScopedValue<'s> {
        ScopedValue::LightUserdata {
            handle: JSON_NULL_LIGHTUSERDATA_HANDLE,
            tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
        }
    }

    /// Interns `bytes` as a Luau string and returns a scope-borrowed handle.
    ///
    /// # Errors
    /// [`RuntimeError::memory`] if interning the string would exceed the VM's memory cap.
    pub fn create_string(&self, bytes: impl AsRef<[u8]>) -> Result<Str<'s>, RuntimeError> {
        let raw = self
            .heap
            .borrow_mut()
            .intern_str(bytes.as_ref())
            .ok_or_else(|| RuntimeError::memory("out of memory creating a string"))?;
        Ok(Str::from_raw(raw))
    }

    /// Interns `bytes` as a table key and returns a VM-lifetime rooted handle.
    ///
    /// Reuse the returned [`KeyHandle`] with [`Table::get_keyed`] and
    /// [`Table::set_keyed`] to update retained tables without converting and
    /// interning the same string key on every write. The handle itself is
    /// registry-rooted, so the weak string interner cannot reclaim the key while
    /// any clone of the handle is live.
    ///
    /// # Errors
    /// [`RuntimeError::memory`] if interning or rooting the key would exceed the
    /// VM's memory cap.
    pub fn intern_key(&self, bytes: impl AsRef<[u8]>) -> Result<KeyHandle, RuntimeError> {
        let mut heap = self.heap.borrow_mut();
        let raw = heap
            .intern_str(bytes.as_ref())
            .ok_or_else(|| RuntimeError::memory("out of memory interning a table key"))?;
        let reference = heap
            .pin(RawValue::String(raw))
            .ok_or_else(|| RuntimeError::memory("out of memory rooting a table key"))?;
        let release = heap.release_sender();
        Ok(KeyHandle::new(raw, Stashed::new(reference, release)))
    }

    /// Allocates a Luau byte buffer initialized from `bytes`.
    ///
    /// # Errors
    /// [`RuntimeError::memory`] if allocating the buffer would exceed the VM's memory cap.
    pub fn create_buffer(&self, bytes: impl AsRef<[u8]>) -> Result<Buffer<'s>, RuntimeError> {
        let raw = self
            .heap
            .borrow_mut()
            .alloc_buffer(LuaBuffer::from_bytes(bytes.as_ref()))
            .ok_or_else(|| RuntimeError::memory("out of memory creating a buffer"))?;
        Ok(Buffer::from_raw(raw))
    }

    /// Wraps `value` as a host userdata of its registered type, returning a
    /// scope-borrowed handle a script can call methods on. The type must have
    /// been registered on the builder
    /// ([`VmBuilder::host_type`](crate::VmBuilder::host_type)); scripts can
    /// never construct one. The heap owns the value: it is dropped when the
    /// instance is collected (or with the VM), and its size is charged against
    /// the memory cap. See this crate's host-userdata docs for the full model.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if `T` is not a registered host type;
    /// [`RuntimeError::memory`] if the allocation would exceed the memory cap.
    pub fn create_userdata<T: Send + 'static>(
        &self,
        value: T,
    ) -> Result<Userdata<'s>, RuntimeError> {
        let mut heap = self.heap.borrow_mut();
        let Some((type_index, entry)) = heap.host_type_for(TypeId::of::<T>()) else {
            return Err(RuntimeError::runtime(format!(
                "host type `{}` is not registered on this VM",
                std::any::type_name::<T>()
            )));
        };
        let payload_size = entry.payload_size;
        let userdata = crate::object::LuaUserdata::new(
            Box::new(crate::host_type::HostCell::new(value)),
            type_index,
            payload_size,
        );
        let raw = heap
            .alloc_userdata(userdata)
            .ok_or_else(|| RuntimeError::memory("out of memory creating a userdata"))?;
        Ok(Userdata::from_raw(raw))
    }

    /// Resolves a userdata handle to its typed payload cell, acquiring the
    /// requested borrow flag while the heap borrow is live. The returned
    /// pointer is valid for the rest of this scope step: the boxed cell's
    /// address is stable and no collection can run while the scope is live.
    /// Every failure — stale handle, foreign type, borrow conflict — is a
    /// catchable [`RuntimeError`] that leaves the VM healthy.
    fn userdata_cell<T: Send + 'static>(
        &self,
        value: Userdata<'s>,
        mode: BorrowMode,
    ) -> Result<std::ptr::NonNull<crate::host_type::HostCell<T>>, RuntimeError> {
        let heap = self.heap.borrow();
        let userdata = heap
            .userdata(value.raw())
            .ok_or_else(|| RuntimeError::runtime("userdata handle no longer resolves"))?;
        let registered_name = || {
            heap.host_types()
                .get(userdata.type_index() as usize)
                .map_or("an unregistered host type", |entry| entry.name.as_str())
        };
        let Some(cell) = userdata
            .cell_any()
            .downcast_ref::<crate::host_type::HostCell<T>>()
        else {
            return Err(RuntimeError::runtime(format!(
                "userdata is not a `{}` (it is '{}')",
                std::any::type_name::<T>(),
                registered_name()
            )));
        };
        let acquired = match mode {
            BorrowMode::Shared => cell.try_borrow_shared(),
            BorrowMode::Exclusive => cell.try_borrow_exclusive(),
        };
        if !acquired {
            let conflict = match mode {
                BorrowMode::Shared => "is already mutably borrowed",
                BorrowMode::Exclusive => "is already borrowed",
            };
            return Err(RuntimeError::runtime(format!(
                "userdata of host type '{}' {conflict}",
                registered_name()
            )));
        }
        Ok(std::ptr::NonNull::from(cell))
    }

    /// Return whether a userdata handle resolves to a payload of type `T`.
    fn userdata_is<T: Send + 'static>(&self, value: Userdata<'s>) -> bool {
        let heap = self.heap.borrow();
        heap.userdata(value.raw())
            .is_some_and(|userdata| userdata.cell_any().is::<crate::host_type::HostCell<T>>())
    }

    /// Copies the bytes behind a string handle.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if the handle no longer resolves in this VM.
    pub fn string_bytes(&self, value: Str<'s>) -> Result<Vec<u8>, RuntimeError> {
        self.heap
            .borrow()
            .string(value.raw())
            .map(|string| string.bytes().to_vec())
            .ok_or_else(|| RuntimeError::runtime("string handle no longer resolves"))
    }

    /// Length of the bytes behind a string handle.
    pub(crate) fn string_len(&self, value: Str<'s>) -> Result<usize, RuntimeError> {
        self.heap
            .borrow()
            .string(value.raw())
            .map(|string| string.len())
            .ok_or_else(|| RuntimeError::runtime("string handle no longer resolves"))
    }

    /// Copies the bytes behind a buffer handle.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if the handle no longer resolves in this VM.
    pub fn buffer_bytes(&self, value: Buffer<'s>) -> Result<Vec<u8>, RuntimeError> {
        self.heap
            .borrow()
            .buffer(value.raw())
            .map(|buffer| buffer.bytes().to_vec())
            .ok_or_else(|| RuntimeError::runtime("buffer handle no longer resolves"))
    }

    /// Length of the bytes behind a buffer handle.
    pub(crate) fn buffer_len(&self, value: Buffer<'s>) -> Result<usize, RuntimeError> {
        self.heap
            .borrow()
            .buffer(value.raw())
            .map(crate::object::LuaBuffer::len)
            .ok_or_else(|| RuntimeError::runtime("buffer handle no longer resolves"))
    }

    /// Copies a scope-borrowed value into an owned [`MarshaledValue`] snapshot.
    ///
    /// This uses the same marshaler and [`Limits`](crate::Limits) value-marshal
    /// caps as owned entry-point results: depth, node count, table-entry count,
    /// string bytes, and buffer bytes all fail closed instead of leaking handles
    /// past the current scope step.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if the value no longer resolves, the value graph
    /// contains a table cycle, a marshal cap is exceeded, or host-side
    /// allocation fails while copying the snapshot.
    pub fn marshal(&self, value: ScopedValue<'s>) -> Result<MarshaledValue, RuntimeError> {
        let heap = self.heap.borrow();
        let limits = ValueMarshalLimits::from(heap.limits());
        let mut visitor = ValueVisitor::new(&heap, limits);
        visitor
            .visit_value(value.into_raw())
            .map_err(|error| RuntimeError::runtime(format!("Scope::marshal failed at {error}")))
    }

    /// Converts a scope-borrowed value into an owned host-return value.
    ///
    /// This is the direct bridge from [`ScopedValue`] to
    /// [`OwnedValue`](ruau_vm_api::OwnedValue): hosts no longer need to manually
    /// stash a value just to return it through an async callback or other owned
    /// host boundary. Immediates and strings are copied directly; heap-backed
    /// values that cannot be represented as plain owned data are registry-pinned
    /// as [`OwnedValue::Pinned`].
    ///
    /// # Errors
    /// [`RuntimeError`] if a string handle no longer resolves or the registry
    /// pin would exceed the VM's memory cap.
    pub fn owned_value(&self, value: ScopedValue<'s>) -> Result<OwnedValue, RuntimeError> {
        Ok(match value {
            ScopedValue::Nil => OwnedValue::Nil,
            ScopedValue::Boolean(value) => OwnedValue::Boolean(value),
            ScopedValue::Number(value) => OwnedValue::Number(value),
            ScopedValue::Integer(value) => OwnedValue::Integer(value),
            ScopedValue::Vector(value) => OwnedValue::Vector(value),
            ScopedValue::LightUserdata { handle, tag } => OwnedValue::LightUserdata { handle, tag },
            ScopedValue::String(value) => OwnedValue::Bytes(self.string_bytes(value)?),
            ScopedValue::Table(_)
            | ScopedValue::Function(_)
            | ScopedValue::Userdata(_)
            | ScopedValue::Thread(_)
            | ScopedValue::Buffer(_) => self.stash_value(value)?.into_owned_value(),
        })
    }

    /// Persists a scope-borrowed table as a [`Stashed`] handle that outlives the
    /// step: the value is registry-rooted, so a later collection cannot reclaim it
    /// while the `Stashed` is live.
    ///
    /// The typed stash surface is deliberately limited to the kinds hosts call
    /// back into — tables ([`Scope::stash_table`] / [`Scope::fetch_table`]) and
    /// functions ([`Scope::stash_function`] / [`Scope::fetch_function`]). Every
    /// other kind stashes through the generic [`Scope::stash_value`] /
    /// [`Scope::fetch_value`] pair.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if the registry pin would exceed the VM's memory cap.
    pub fn stash_table(&self, value: Table<'s>) -> Result<Stashed<marker::Table>, RuntimeError> {
        let mut heap = self.heap.borrow_mut();
        let reference = heap
            .pin(RawValue::Table(value.raw()))
            .ok_or_else(|| RuntimeError::memory("out of memory stashing a value"))?;
        let release = heap.release_sender();
        Ok(Stashed::new(reference, release))
    }

    /// Persists a scope-borrowed function as a [`Stashed`] callback handle.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if the registry pin would exceed the VM's memory cap.
    pub fn stash_function(
        &self,
        value: Function<'s>,
    ) -> Result<Stashed<marker::Closure>, RuntimeError> {
        let mut heap = self.heap.borrow_mut();
        let reference = heap
            .pin(RawValue::Function(value.handle.raw()))
            .ok_or_else(|| RuntimeError::memory("out of memory stashing a function"))?;
        let release = heap.release_sender();
        Ok(Stashed::new(reference, release))
    }

    /// Re-acquires the live table behind a [`Stashed`] as a scope-borrowed handle
    /// for the duration of this step. The round trip of [`Scope::stash_table`] — pass a
    /// `Stashed` minted in an earlier step and read its value here.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if the pin no longer resolves (it was released, or its
    /// generation is stale) or the stashed value is not a table.
    pub fn fetch_table(&self, stashed: &Stashed<marker::Table>) -> Result<Table<'s>, RuntimeError> {
        let raw = self
            .heap
            .borrow()
            .pinned_value(stashed.reference())
            .map_err(RuntimeError::runtime)?;
        match raw {
            RawValue::Table(handle) => Ok(Table::from_raw(handle)),
            _ => Err(RuntimeError::runtime("stashed value is not a table")),
        }
    }

    /// Re-acquires the live function behind a [`Stashed`] as a scope-borrowed
    /// callback for the duration of this step.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if the pin no longer resolves or the stashed value is
    /// not a function.
    pub fn fetch_function(
        &self,
        stashed: &Stashed<marker::Closure>,
    ) -> Result<Function<'s>, RuntimeError> {
        let raw = self
            .heap
            .borrow()
            .pinned_value(stashed.reference())
            .map_err(RuntimeError::runtime)?;
        match raw {
            RawValue::Function(handle) => Ok(Function::from_raw(handle)),
            _ => Err(RuntimeError::runtime("stashed value is not a function")),
        }
    }

    /// Persists **any** scope-borrowed value as a [`Stashed`] handle that
    /// outlives the step — the generic companion to the typed [`Scope::stash_table`] /
    /// [`Scope::stash_function`], with the same rooting semantics: the value is
    /// registry-pinned, so a collection cannot reclaim it while the `Stashed`
    /// is live. Immediates (`nil`, booleans, numbers, vectors, light userdata)
    /// stash uniformly; their pin simply roots nothing.
    ///
    /// # Errors
    /// [`RuntimeError::memory`] if the registry pin would exceed the VM's memory cap.
    pub fn stash_value(
        &self,
        value: ScopedValue<'s>,
    ) -> Result<Stashed<crate::marker::Value>, RuntimeError> {
        let mut heap = self.heap.borrow_mut();
        let reference = heap
            .pin(value.into_raw())
            .ok_or_else(|| RuntimeError::memory("out of memory stashing a value"))?;
        let release = heap.release_sender();
        Ok(Stashed::new(reference, release))
    }

    /// Re-acquires the live value behind a generic [`Stashed`] as a
    /// scope-borrowed [`ScopedValue`] for the duration of this step — the round
    /// trip of [`Scope::stash_value`]. The value comes back as whatever kind was
    /// stashed.
    ///
    /// # Errors
    /// [`RuntimeError::runtime`] if the pin no longer resolves (it was released, or
    /// its generation is stale).
    pub fn fetch_value(
        &self,
        stashed: &Stashed<crate::marker::Value>,
    ) -> Result<ScopedValue<'s>, RuntimeError> {
        let raw = self
            .heap
            .borrow()
            .pinned_value(stashed.reference())
            .map_err(RuntimeError::runtime)?;
        Ok(ScopedValue::from_raw(raw))
    }

    /// The source position of the `level`-th Lua frame on the current thread's
    /// call stack, innermost first — from inside a host function, level 0 is the
    /// Lua frame whose call invoked it (the call site), level 1 is that frame's
    /// Lua caller, and so on.
    ///
    /// Levels count executable **Lua** frames only: native activations (host
    /// functions and engine builtins run without a Lua frame) and
    /// `pcall`/`require` boundaries are transparent, so `f -> pcall -> host`
    /// still reports `f`'s call site at level 0. Luau performs no tail-call
    /// elimination, so every active Lua call contributes exactly one frame.
    ///
    /// For an async host function, capture the location at the synchronous
    /// front of the call (while the `Scope` is live, before the future is
    /// returned); by the time the future runs, the caller may have been
    /// suspended or resumed elsewhere.
    ///
    /// Returns `None` past the stack top (including a [`Vm::step`](crate::Vm::step)
    /// scope with no Lua frames below it) and for a frame whose chunk was
    /// loaded without line info.
    #[must_use]
    pub fn caller_location(&self, level: usize) -> Option<SourceLocation> {
        let heap = self.heap.borrow();
        let thread = self.thread.borrow();
        crate::debug::caller_location(&heap, &thread, level)
    }

    /// Re-acquires a loaded module's main closure as a scope-borrowed callable,
    /// so a host call can run a module the host compiled and loaded earlier (a
    /// bound script) through [`Scope::call`] / [`Scope::call_protected`] while
    /// the VM is already borrowed by the running script.
    ///
    /// The module's own registry pin keeps the closure rooted for as long as the
    /// host holds the [`LoadedModule`], and [`Vm::unload`](crate::Vm::unload)
    /// consumes the module by value, so the handle cannot dangle. Like
    /// `Vm::call_function`, this is a trusted
    /// embedder entry point: a module loaded into a *different* VM is rejected by
    /// handle validation when called, not a memory-safety hazard.
    #[must_use]
    pub fn module_function(&self, module: &LoadedModule) -> Function<'s> {
        Function::from_raw(module.main)
    }

    /// Compiles `source` at runtime and returns its main closure without
    /// running it.
    ///
    /// Uses the VM's installed [`RuntimeCompiler`](crate::RuntimeCompiler),
    /// runtime-compilation limits, cancellation token, and load validation.
    /// The chunk is bound to the current thread's global environment. The
    /// returned handle is valid for this scope step; stash it to keep it.
    ///
    /// `chunk_name` is the raw name used in error locations. `=name` and
    /// `@name` display as `name`; a bare name displays as `[string "name"]`.
    /// Use [`ChunkName`](crate::ChunkName) to construct or inspect these bytes
    /// without hand-formatting markers.
    ///
    /// # Errors
    /// [`RuntimeError`] if runtime compilation is disabled, a cap is exceeded,
    /// the source is malformed, or the compiled chunk fails validation.
    pub fn load_chunk(
        &self,
        source: &[u8],
        chunk_name: &[u8],
    ) -> Result<Function<'s>, RuntimeError> {
        let mut heap = self.heap.borrow_mut();
        if !heap.runtime_compilation_enabled() {
            return Err(RuntimeError::runtime(
                "runtime compilation is disabled: the VM's runtime capabilities do not enable it \
                 (see RuntimeCapabilities::enable_runtime_compilation)",
            ));
        }
        let limits = heap.limits();
        let compiler = heap.runtime_compiler();
        let context = heap.runtime_compile_context();
        let chunk = compiler
            .compile(source, context)
            .map_err(|message| chunk_compile_error(chunk_name, &message))?;
        let module = crate::load::load_with_limits(
            &mut heap,
            &chunk,
            crate::load::LoadMode::Validated,
            chunk_name,
            limits,
        )
        .map_err(|error| chunk_load_error(chunk_name, &error))?;
        // Like `loadstring`: the chunk runs in the current thread's global
        // environment (under `sandbox_thread`, the thread's writable proxy).
        if let Some(closure) = heap.closure_mut(module.main) {
            closure.env = self.thread.borrow().globals;
        }
        let function = Function::from_raw(module.main);
        // Release the loader's pin, like `loadstring`: the returned handle is
        // scope-branded, and the host stashes it to root it past this step.
        heap.unpin(&module.pin);
        Ok(function)
    }

    /// Compiles and runs `source` with no arguments.
    ///
    /// This is [`Scope::load_chunk`] followed by [`Scope::call`].
    ///
    /// # Errors
    /// [`RuntimeError`] for any [`Scope::load_chunk`] failure, or if the chunk
    /// raises (carrying the failure's [`ruau_vm_api::RuntimeErrorKind`]).
    pub fn eval_chunk(
        &self,
        source: &[u8],
        chunk_name: &[u8],
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let function = self.load_chunk(source, chunk_name)?;
        let results = self.call_raw(RawValue::Function(function.raw()), &[])?;
        Ok(MultiValue::from_raw_values(results))
    }

    pub(crate) fn pin_raw_values(
        &self,
        values: &[RawValue],
    ) -> Result<Vec<RegistryRef>, RuntimeError> {
        let mut roots = Vec::new();
        roots
            .try_reserve(values.len())
            .map_err(|_| RuntimeError::memory("out of memory rooting scoped values"))?;
        let mut heap = self.heap.borrow_mut();
        for &value in values {
            match heap.pin(value) {
                Some(reference) => roots.push(reference),
                None => {
                    for reference in roots.drain(..) {
                        heap.unpin(&reference);
                    }
                    return Err(RuntimeError::memory("out of memory rooting scoped values"));
                }
            }
        }
        Ok(roots)
    }

    /// Roots a table under a host-chosen name, so a later step (or a later run on
    /// the same VM) can re-acquire it by name with [`Scope::named_get`]. Replaces
    /// and releases any previous value at that name. The named registry is host
    /// state — an untrusted script cannot reach it.
    ///
    /// # Errors
    /// [`RuntimeError::memory`] if rooting the value would exceed the VM's memory cap.
    pub fn named_set(&self, key: &[u8], value: Table<'s>) -> Result<(), RuntimeError> {
        self.heap
            .borrow_mut()
            .named_set(key, RawValue::Table(value.raw()))
            .ok_or_else(|| RuntimeError::memory("out of memory storing a named value"))
    }

    /// Re-acquires the table rooted under `key`, or `None` if absent (or not a
    /// table).
    #[must_use]
    pub fn named_get(&self, key: &[u8]) -> Option<Table<'s>> {
        match self.heap.borrow().named_get(key)? {
            RawValue::Table(handle) => Some(Table::from_raw(handle)),
            _ => None,
        }
    }

    /// Releases the value rooted under `key`, returning whether one was present.
    pub fn named_remove(&self, key: &[u8]) -> bool {
        self.heap.borrow_mut().named_remove(key)
    }

    /// Borrows typed host state installed on the VM with
    /// [`Vm::set_app_data`](crate::Vm::set_app_data), if present. The guard derefs to
    /// `&T` and borrows only the app-data cell, so it composes with value
    /// construction (`create_table`, …) on the same step — only holding it across an
    /// `app_data_mut` of the same cell conflicts.
    #[must_use]
    pub fn app_data<T: Any>(&self) -> Option<Ref<'_, T>> {
        Ref::filter_map(self.app_data.try_borrow().ok()?, |data| data.get::<T>()).ok()
    }

    /// Mutably borrows typed host state installed on the VM, if present.
    #[must_use]
    pub fn app_data_mut<T: Any>(&self) -> Option<RefMut<'_, T>> {
        RefMut::filter_map(self.app_data.try_borrow_mut().ok()?, |data| {
            data.get_mut::<T>()
        })
        .ok()
    }

    /// Whether this scope has a borrowed host context installed.
    #[must_use]
    pub fn has_context(&self) -> bool {
        self.context.is_some()
    }

    /// Mutably borrows the host context lent to this VM entry.
    ///
    /// The context is borrowed, not owned, and does not need to be `Send` or
    /// `Sync`. It lasts exactly as long as the VM entry that installed it.
    #[must_use]
    pub fn context_mut<T: Any>(&self) -> Option<ContextMut<'_, T>> {
        self.context?.borrow_mut::<T>()
    }

    /// Calls a Luau function **synchronously and non-yieldably** from the host,
    /// converting arguments and results through the scoped value model. The call
    /// runs as a native nested invocation (`call::run_function`, dispatch mode
    /// `Nested`): it never collects, never preempts, and a callee that tries to
    /// await an async host gets a clear error rather than suspending. An uncaught
    /// script error unwinds cleanly and the VM stays usable.
    ///
    /// # Errors
    /// [`RuntimeError`] if argument/result conversion fails or if the called function
    /// raises.
    pub fn call<A, R>(&self, func: Function<'s>, args: A) -> Result<R, RuntimeError>
    where
        A: IntoLuaMulti<'s>,
        R: FromLuaMulti<'s>,
    {
        let args = args.into_lua_multi(self)?.into_raw_vec();
        let results = self.call_raw(RawValue::Function(func.handle.raw()), &args)?;
        R::from_lua_multi(MultiValue::from_raw_values(results), self)
    }

    /// Calls a Luau function synchronously in a protected scope, converting
    /// arguments and successful results through the scoped value model.
    ///
    /// This first protected-call layer uses the same non-yieldable nested
    /// dispatch as [`Scope::call`]. A callee that tries to await an async host
    /// still errors cleanly; the suspendable protected path is built on the async
    /// driver.
    ///
    /// Catchable script errors return as the inner [`Err`] with the materialized
    /// Lua error value. Fatal control-flow categories, such as cancellation,
    /// deadline, or VM poison, are not catchable and return as the outer
    /// [`RuntimeError`].
    ///
    /// # Errors
    /// The outer [`RuntimeError`] covers argument/result conversion failures and fatal
    /// uncatchable VM failures. Catchable script failures are returned as the
    /// inner [`ScriptError`].
    pub fn call_protected<A, R>(
        &self,
        func: Function<'s>,
        args: A,
    ) -> Result<Result<R, ScriptError<'s>>, RuntimeError>
    where
        A: IntoLuaMulti<'s>,
        R: FromLuaMulti<'s>,
    {
        let args = args.into_lua_multi(self)?.into_raw_vec();
        let protected = {
            let mut heap = self.heap.borrow_mut();
            let mut thread = self.thread.borrow_mut();
            match call::protected_call_with_traceback(
                &mut heap,
                &mut thread,
                RawValue::Function(func.handle.raw()),
                &args,
                crate::SCRIPT_ERROR_TRACEBACK_MAX_BYTES,
            ) {
                Ok(results) => Ok(Ok(MultiValue::from_raw_values(results))),
                Err(failure) if failure.error.is_catchable() => {
                    let kind = failure.error.kind;
                    let traceback = failure.traceback;
                    let capture = thread.captured_traceback.take();
                    let in_flight = failure.error.host_payload.clone();
                    let value = call::materialize(&mut heap, failure.error);
                    let payload = call::recover_host_payload(&heap, in_flight, value);
                    Ok(Err(ScriptError::new(ScopedValue::from_raw(value), kind)
                        .with_traceback(traceback, capture)
                        .with_host_payload(payload)))
                }
                Err(failure) => Err(RuntimeError::from_uncatchable_protected_kind(
                    failure.error.kind,
                )
                .with_host_payload(failure.error.host_payload)),
            }
        }?;
        match protected {
            Ok(results) => R::from_lua_multi(results, self).map(Ok),
            Err(error) => Ok(Err(error)),
        }
    }

    /// Calls a Luau function **synchronously and non-yieldably** with raw values.
    ///
    /// This is the low-level escape hatch for VM-internal tests and engine-facing
    /// code. General embedders should prefer [`Scope::call`], which brands
    /// handles and runs conversions through `IntoLua`/`FromLua`.
    ///
    /// A returned raw handle is valid only until the next nested call: if that
    /// call resumes a coroutine, the coroutine body may collect, reclaiming any
    /// heap value not reachable from a root. [`stash_table`](Scope::stash_table) a handle to
    /// hold it across calls. The reclamation is memory-safe — a stale `RawGc`
    /// resolves to absent, not to garbage — but the value is gone.
    ///
    /// # Errors
    /// [`RuntimeError`] carrying the script error's [`RuntimeErrorKind`] if the call raises.
    pub(crate) fn call_raw(
        &self,
        func: RawValue,
        args: &[RawValue],
    ) -> Result<Vec<RawValue>, RuntimeError> {
        let mut heap = self.heap.borrow_mut();
        let mut thread = self.thread.borrow_mut();
        call::run_function(&mut heap, &mut thread, func, args).map_err(|unwind| {
            let payload = call::recover_host_payload(&heap, None, unwind.error);
            RuntimeError::from_unwind(&unwind).with_host_payload(payload)
        })
    }

    /// Looks up a global by name in the lane's global table, or `None` if it is
    /// absent (`nil`). This is the raw companion to [`Scope::global_value`];
    /// general embedders should prefer the typed path.
    #[must_use]
    pub(crate) fn global(&self, name: &[u8]) -> Option<RawValue> {
        let mut heap = self.heap.borrow_mut();
        let globals = self.thread.borrow().globals?;
        let key = heap.intern_str(name)?;
        match heap.table(globals)?.get(RawValue::String(key)) {
            RawValue::Nil => None,
            value => Some(value),
        }
    }

    /// Looks up a global by name and returns it as a scope-branded value, or
    /// `None` if it is absent (`nil`).
    #[must_use]
    pub fn global_value(&self, name: &[u8]) -> Option<ScopedValue<'s>> {
        self.global(name).map(ScopedValue::from_raw)
    }

    /// Looks up a global function by name, returning `None` when the global is
    /// absent or not callable.
    #[must_use]
    pub fn global_function(&self, name: &[u8]) -> Option<Function<'s>> {
        match self.global_value(name)? {
            ScopedValue::Function(function) => Some(function),
            _ => None,
        }
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        if !self.exit_scope_on_drop {
            return;
        }
        // `try_borrow_mut` rather than `borrow_mut`: a borrow is never held across a
        // call today (every method's borrow is statement-scoped), but if a future
        // method ever unwinds while holding one, a `borrow_mut` here would turn the
        // unwind into a double-panic abort. On the (currently unreachable) failure
        // the guard stays set, which fails the next step closed — never silently.
        if let Ok(mut heap) = self.heap.try_borrow_mut() {
            heap.exit_scope();
        }
    }
}

#[cfg(any())]
mod tests {
    use ruau_vm_api::{HeapId, marker};

    use super::*;

    #[test]
    fn error_constructors_carry_message_and_runtime_kind() {
        let external = RuntimeError::external("disk is full");
        assert_eq!(external.message(), "disk is full");
        assert_eq!(external.kind(), RuntimeErrorKind::Runtime);
        assert_eq!(external.to_string(), "disk is full");

        let runtime = RuntimeError::runtime(format!("bad index {}", 7));
        assert_eq!(runtime.message(), "bad index 7");
        // It composes as a std::error::Error.
        let _as_error: &dyn std::error::Error = &runtime;
    }

    #[test]
    fn stashed_is_kind_marked_and_releases_its_pin_only_on_last_drop() {
        let reference = RegistryRef::from_parts(3, 1, HeapId(9));
        let (tx, rx) = std::sync::mpsc::channel();
        let stashed: Stashed<marker::Table> = Stashed::new(reference.clone(), tx);
        assert_eq!(stashed.reference(), &reference);

        // A clone shares the one pin; dropping it does *not* enqueue a release.
        let clone = stashed.clone();
        assert_eq!(clone.reference(), &reference);
        drop(clone);
        assert!(
            rx.try_recv().is_err(),
            "a release must not fire while a clone is still live"
        );

        // Dropping the last clone enqueues the pin for release.
        drop(stashed);
        assert_eq!(rx.try_recv().ok(), Some(reference));
    }

    // A compile-time witness that the intended return types satisfy `IntoStash`.
    // The negative cases live in the trybuild UI tests.
    #[test]
    fn into_stash_admits_owned_returns() {
        fn assert_into_stash<T: IntoStash>() {}
        assert_into_stash::<()>();
        assert_into_stash::<bool>();
        assert_into_stash::<i64>();
        assert_into_stash::<f64>();
        assert_into_stash::<String>();
        assert_into_stash::<Vec<u8>>();
        assert_into_stash::<OwnedValue>();
        assert_into_stash::<MarshaledValue>();
        assert_into_stash::<Stashed<marker::Str>>();
        assert_into_stash::<Option<i64>>();
        assert_into_stash::<Result<i64, String>>();
        assert_into_stash::<(i64, Stashed<marker::Table>)>();
    }

    #[test]
    fn scope_marshal_snapshots_tables_and_buffers() {
        let mut vm = crate::test_vm();

        let (table, buffer) = vm
            .step(|s| {
                let table = s.create_table()?;
                table.set(s, "answer", 42_i64)?;
                table.set(s, 1_i64, "first")?;
                let buffer = s.create_buffer(b"bytes")?;
                Ok((
                    s.marshal(ScopedValue::Table(table))?,
                    s.marshal(ScopedValue::Buffer(buffer))?,
                ))
            })
            .expect("scope marshals values");

        let MarshaledValue::Table(pairs) = table else {
            panic!("expected table snapshot, got {table:?}");
        };
        assert!(
            pairs.iter().any(|pair| {
                matches!(
                    (&pair.key, &pair.value),
                    (MarshaledValue::String(key), MarshaledValue::Integer(42))
                        if key == b"answer"
                )
            }),
            "{pairs:?}"
        );
        assert!(
            pairs.iter().any(|pair| {
                matches!(
                    (&pair.key, &pair.value),
                    (MarshaledValue::Integer(1), MarshaledValue::String(value))
                        if value == b"first"
                )
            }),
            "{pairs:?}"
        );
        assert_eq!(buffer, MarshaledValue::Buffer(b"bytes".to_vec()));
    }

    #[test]
    fn scoped_values_convert_directly_to_owned_values() {
        let mut vm = crate::test_vm();

        let (string, table, buffer) = vm
            .step(|s| {
                let text = s.create_string(b"owned text")?;
                let table = s.create_table()?;
                table.set(s, "answer", 42_i64)?;
                let buffer = s.create_buffer(b"bytes")?;
                Ok((
                    ScopedValue::String(text).to_owned_value(s)?,
                    s.owned_value(ScopedValue::Table(table))?,
                    s.owned_value(ScopedValue::Buffer(buffer))?,
                ))
            })
            .expect("scope converts values");

        assert!(
            matches!(&string, OwnedValue::Bytes(bytes) if bytes == b"owned text"),
            "{string:?}"
        );
        let OwnedValue::Pinned(table_ref) = table else {
            panic!("table should become a pinned owned value, got {table:?}");
        };
        let OwnedValue::Pinned(buffer_ref) = buffer else {
            panic!("buffer should become a pinned owned value, got {buffer:?}");
        };
        assert!(matches!(
            vm.heap()
                .pinned_value(&table_ref)
                .expect("table pin resolves"),
            RawValue::Table(_)
        ));
        assert!(matches!(
            vm.heap()
                .pinned_value(&buffer_ref)
                .expect("buffer pin resolves"),
            RawValue::Buffer(_)
        ));
    }

    #[test]
    fn scoped_value_display_covers_scope_borrowed_kinds() {
        let mut vm = crate::test_vm();

        vm.step(|s| {
            let text = s.create_string(b"hello")?;
            let table = s.create_table()?;
            let buffer = s.create_buffer(b"bytes")?;
            let values = [
                (ScopedValue::Nil, "nil"),
                (ScopedValue::Boolean(false), "false"),
                (ScopedValue::Number(2.0), "2"),
                (ScopedValue::Number(-0.0), "-0"),
                (ScopedValue::Integer(4), "4"),
                (ScopedValue::Vector([1.0, 2.5, -0.0]), "1, 2.5, -0"),
                (ScopedValue::LightUserdata { handle: 1, tag: 2 }, "userdata"),
                (ScopedValue::String(text), "hello"),
                (ScopedValue::Table(table), "table"),
                (ScopedValue::Buffer(buffer), "buffer"),
            ];

            for (value, display) in values {
                assert_eq!(value.display(s), display, "{value:?}");
            }
            Ok(())
        })
        .expect("scope displays values");
    }

    #[test]
    fn scope_marshal_enforces_depth_limit() {
        let mut vm = crate::Vm::builder()
            .limits(crate::Limits {
                max_value_marshal_depth: Some(1),
                ..crate::Limits::unlimited()
            })
            .build_for_test();

        let error = vm
            .step(|s| {
                let outer = s.create_table()?;
                let inner = s.create_table()?;
                outer.set(s, "inner", inner)?;
                s.marshal(ScopedValue::Table(outer))
            })
            .expect_err("nested table exceeds depth cap");

        assert_eq!(error.kind(), RuntimeErrorKind::Runtime);
        assert!(
            error
                .message()
                .contains("value depth exceeds marshal cap 1"),
            "{error}"
        );
        assert!(error.message().contains("$.pair"), "{error}");
    }

    #[test]
    fn scope_marshal_rejects_table_cycles() {
        let mut vm = crate::test_vm();

        let error = vm
            .step(|s| {
                let table = s.create_table()?;
                table.set(s, "self", table)?;
                s.marshal(ScopedValue::Table(table))
            })
            .expect_err("cyclic table cannot be marshaled");

        assert_eq!(error.kind(), RuntimeErrorKind::Runtime);
        assert!(
            error.message().contains("table cycle cannot be marshaled"),
            "{error}"
        );
        assert!(error.message().contains("$.pair"), "{error}");
    }

    #[test]
    fn a_stashed_table_survives_to_a_later_step_and_is_fetchable() {
        let mut vm = crate::test_vm();
        // One step builds and persists a table...
        let stashed = vm
            .step(|s| {
                let table = s.create_table()?;
                s.stash_table(table)
            })
            .expect("step builds and stashes a table");
        // ...and a *later* step re-acquires the live value through the stash. The
        // fetched handle is scope-borrowed, so it cannot itself escape this step.
        let fetched_ok = vm
            .step(|s| {
                let _table = s.fetch_table(&stashed)?;
                Ok(true)
            })
            .expect("a later step fetches the stashed table");
        assert!(fetched_ok);
        // The underlying pin still resolves to a table.
        let value = vm
            .heap()
            .pinned_value(stashed.reference())
            .expect("the stashed pin still resolves");
        assert!(matches!(value, RawValue::Table(_)));
    }

    #[test]
    fn stash_value_round_trips_immediates_uniformly_across_steps() {
        let mut vm = crate::test_vm();
        // Every immediate kind stashes through the same API; the pins resolve in
        // a later step with kind and payload intact.
        let stashed = vm
            .step(|s| {
                Ok((
                    s.stash_value(ScopedValue::Nil)?,
                    s.stash_value(ScopedValue::Boolean(true))?,
                    (
                        s.stash_value(ScopedValue::Number(2.5))?,
                        s.stash_value(ScopedValue::Integer(-7))?,
                        (
                            s.stash_value(ScopedValue::Vector([1.0, 2.0, 3.0]))?,
                            s.stash_value(ScopedValue::LightUserdata { handle: 9, tag: 3 })?,
                        ),
                    ),
                ))
            })
            .expect("stash immediates");
        let (nil, boolean, (number, integer, (vector, light))) = stashed;
        vm.step(|s| {
            assert!(matches!(s.fetch_value(&nil)?, ScopedValue::Nil));
            assert!(matches!(
                s.fetch_value(&boolean)?,
                ScopedValue::Boolean(true)
            ));
            assert!(matches!(s.fetch_value(&number)?, ScopedValue::Number(n) if n == 2.5));
            assert!(matches!(s.fetch_value(&integer)?, ScopedValue::Integer(-7)));
            assert!(
                matches!(s.fetch_value(&vector)?, ScopedValue::Vector(v) if v == [1.0, 2.0, 3.0])
            );
            assert!(matches!(
                s.fetch_value(&light)?,
                ScopedValue::LightUserdata { handle: 9, tag: 3 }
            ));
            Ok(())
        })
        .expect("fetch immediates in a later step");
    }

    #[test]
    fn stash_value_roots_heap_kinds_across_a_collection() {
        let mut vm = crate::test_vm();
        // One step builds and stashes a string, a table, and a buffer; nothing
        // else roots them.
        let (string, table, buffer) = vm
            .step(|s| {
                let string = s.stash_value("stashed text".into_lua(s)?)?;
                let t = s.create_table()?;
                t.set(s, "key", 41_i64)?;
                let table = s.stash_value(ScopedValue::Table(t))?;
                let buffer = s.stash_value(ScopedValue::Buffer(s.create_buffer([4, 5, 6])?))?;
                Ok((string, table, buffer))
            })
            .expect("stash heap kinds");
        // A full collection between steps must not reclaim them — the generic
        // stash pins like the typed ones.
        vm.collect();
        vm.step(|s| {
            let ScopedValue::String(text) = s.fetch_value(&string)? else {
                panic!("stashed string came back as the wrong kind");
            };
            assert_eq!(s.string_bytes(text)?, b"stashed text");
            let ScopedValue::Table(t) = s.fetch_value(&table)? else {
                panic!("stashed table came back as the wrong kind");
            };
            assert_eq!(t.get::<_, i64>(s, "key")?, 41);
            let ScopedValue::Buffer(b) = s.fetch_value(&buffer)? else {
                panic!("stashed buffer came back as the wrong kind");
            };
            assert_eq!(b.to_vec(s)?, vec![4, 5, 6]);
            Ok(())
        })
        .expect("fetch heap kinds after a collection");
    }

    #[test]
    fn a_dropped_value_stash_releases_its_pin_even_for_nil() {
        let mut vm = crate::test_vm();
        let stashed = vm
            .step(|s| s.stash_value(ScopedValue::Nil))
            .expect("stash nil");
        let reference = stashed.reference().clone();
        // A pinned nil is a live registry slot (liveness is the token, not the
        // value), so it resolves to Nil while the stash is held...
        assert_eq!(vm.heap().pinned_value(&reference), Ok(RawValue::Nil));
        // ...and releases like any other stash once the last clone drops.
        drop(stashed);
        vm.step(|_s| Ok(())).expect("drain step");
        assert!(vm.heap().pinned_value(&reference).is_err());
    }

    #[test]
    fn caller_location_is_none_with_no_lua_frames() {
        let mut vm = crate::test_vm();
        // A plain `Vm::step` scope has no Lua frames below it.
        let location = vm.step(|s| Ok(s.caller_location(0))).expect("step");
        assert_eq!(location, None);
    }

    #[test]
    fn app_data_is_readable_and_mutable_across_steps() {
        #[derive(Debug)]
        struct Counter(u32);

        let mut vm = crate::test_vm();
        vm.set_app_data(Counter(0));

        // A held app-data read guard composes with value construction on the same
        // step — app data is in its own cell, so this does not double-borrow the
        // heap (the footgun the round-3 review caught and this guards against).
        vm.step(|s| {
            let counter = s.app_data::<Counter>().expect("installed");
            let _table = s.create_table()?; // would panic if app data shared the heap cell
            assert_eq!(counter.0, 0);
            Ok(())
        })
        .expect("step");
        // A read guard and an `app_data_mut` of the *same* cell still conflict (the
        // ordinary RefCell discipline), so the read is statement-scoped here.
        vm.step(|s| {
            assert_eq!(s.app_data::<Counter>().expect("installed").0, 0);
            s.app_data_mut::<Counter>().expect("installed").0 += 5;
            Ok(())
        })
        .expect("step");

        // The mutation persisted, and an uninstalled type is absent.
        let five = vm
            .step(|s| {
                let n = s.app_data::<Counter>().expect("installed").0;
                Ok(n == 5 && s.app_data::<String>().is_none())
            })
            .expect("step");
        assert!(five);

        // Removing it makes it absent.
        assert!(vm.remove_app_data::<Counter>());
        let gone = vm
            .step(|s| Ok(s.app_data::<Counter>().is_none()))
            .expect("step");
        assert!(gone);
    }

    #[test]
    fn app_data_conflicting_borrows_return_none() {
        struct Counter(u32);

        let mut vm = crate::test_vm();
        vm.set_app_data(Counter(0));

        vm.step(|s| {
            let shared = s.app_data::<Counter>().expect("installed");
            assert!(
                s.app_data_mut::<Counter>().is_none(),
                "a conflicting mutable borrow is absence, not a panic"
            );
            assert_eq!(shared.0, 0);
            drop(shared);

            let mutable = s.app_data_mut::<Counter>().expect("installed");
            assert!(
                s.app_data::<Counter>().is_none(),
                "a conflicting shared borrow is absence, not a panic"
            );
            assert_eq!(mutable.0, 0);
            Ok(())
        })
        .expect("step");
    }

    #[test]
    fn clear_named_registry_releases_all_named_entries() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let table = s.create_table()?;
            s.named_set(b"a", table)
        })
        .expect("set a");
        vm.step(|s| {
            let table = s.create_table()?;
            s.named_set(b"b", table)
        })
        .expect("set b");
        let both = vm
            .step(|s| Ok(s.named_get(b"a").is_some() && s.named_get(b"b").is_some()))
            .expect("step");
        assert!(both, "both named entries are present before the clear");

        vm.clear_named_registry();
        let gone = vm
            .step(|s| Ok(s.named_get(b"a").is_none() && s.named_get(b"b").is_none()))
            .expect("step");
        assert!(gone, "clear_named_registry releases all named entries");
        // The now-unpinned tables are reclaimable; a collection stays consistent.
        vm.collect();
    }

    #[test]
    fn dropping_a_stash_releases_its_pin_on_the_next_step() {
        let mut vm = crate::test_vm();
        let stashed = vm
            .step(|s| {
                let table = s.create_table()?;
                s.stash_table(table)
            })
            .expect("stash");
        let reference = stashed.reference().clone();
        assert!(
            vm.heap().pinned_value(&reference).is_ok(),
            "the pin resolves while the stash is live"
        );

        // Drop the only clone, then take a step: the step drains the release and
        // unpins, so the pin no longer resolves.
        drop(stashed);
        vm.step(|_s| Ok(())).expect("drain step");
        assert!(
            vm.heap().pinned_value(&reference).is_err(),
            "the dropped stash's pin was released by the next step's drain"
        );
    }

    #[test]
    fn the_named_registry_roots_a_value_across_steps_and_a_collection() {
        let mut vm = crate::test_vm();
        // One step roots a table under a name.
        vm.step(|s| {
            let table = s.create_table()?;
            s.named_set(b"session", table)
        })
        .expect("named_set");
        // A full collection between steps must not reclaim it — the name roots it.
        vm.collect();
        // A later step re-acquires it by name.
        let found = vm
            .step(|s| Ok(s.named_get(b"session").is_some()))
            .expect("named_get step");
        assert!(found, "the named value was reclaimed across a collection");
        // Removing it releases the root, and it is then absent.
        let removed = vm.step(|s| Ok(s.named_remove(b"session"))).expect("remove");
        assert!(removed);
        let gone = vm
            .step(|s| Ok(s.named_get(b"session").is_none()))
            .expect("absent step");
        assert!(gone);
    }

    #[test]
    fn a_borrowed_handle_debug_is_opaque() {
        // Format the handle *inside* the step (a `String` is `IntoStash`, so it may
        // leave). The rendered form must carry no raw index/generation/heap, or a
        // step could smuggle the handle's identity out and rebuild it with
        // `RawGc::from_parts`, escaping the brand.
        let mut vm = crate::test_vm();
        let rendered = vm
            .step(|s| {
                let table = s.create_table()?;
                Ok(format!("{table:?}"))
            })
            .expect("step");
        assert!(
            !rendered.chars().any(|c| c.is_ascii_digit()),
            "borrowed-handle Debug leaked raw parts: {rendered}"
        );
    }

    #[test]
    fn value_debug_is_opaque_for_heap_handles() {
        let mut vm = crate::test_vm();
        let rendered = vm
            .step(|s| {
                let string = "hello".into_lua(s)?;
                let table = s.create_table()?.into_lua(s)?;
                let buffer = s.create_buffer([1, 2, 3])?.into_lua(s)?;
                Ok(format!("{string:?} {table:?} {buffer:?}"))
            })
            .expect("step");
        assert!(
            !rendered.chars().any(|c| c.is_ascii_digit()),
            "ScopedValue Debug leaked raw parts: {rendered}"
        );
        assert!(rendered.contains("String { .. }"));
        assert!(rendered.contains("Table { .. }"));
        assert!(rendered.contains("Buffer { .. }"));
    }

    #[test]
    fn scalar_string_and_option_conversions_round_trip_in_a_scope() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            assert!(bool::from_lua(true.into_lua(s)?, s)?);
            assert_eq!(i8::from_lua(12_i8.into_lua(s)?, s)?, 12);
            assert_eq!(i32::from_lua(42_i32.into_lua(s)?, s)?, 42);
            assert_eq!(i64::from_lua(42_i64.into_lua(s)?, s)?, 42);
            assert_eq!(i8::from_lua(ScopedValue::Number(-5.0), s)?, -5);
            assert_eq!(i64::from_lua(ScopedValue::Number(42.0), s)?, 42);
            assert_eq!(u16::from_lua(64_u16.into_lua(s)?, s)?, 64);
            assert_eq!(u8::from_lua(ScopedValue::Number(255.0), s)?, 255);
            assert_eq!(usize::from_lua(3_usize.into_lua(s)?, s)?, 3);
            assert_eq!(f32::from_lua(1.25_f32.into_lua(s)?, s)?, 1.25);
            assert_eq!(f64::from_lua(2.5_f64.into_lua(s)?, s)?, 2.5);
            assert_eq!(
                Option::<i64>::from_lua(Option::<i64>::None.into_lua(s)?, s)?,
                None
            );
            assert_eq!(
                Option::<i64>::from_lua(Some(7_i64).into_lua(s)?, s)?,
                Some(7)
            );

            let text = "scope text".into_lua(s)?;
            assert!(matches!(text, ScopedValue::String(_)));
            assert_eq!(String::from_lua(text, s)?, "scope text");

            let bytes = b"raw bytes".as_slice().into_lua(s)?;
            let ScopedValue::String(bytes) = bytes else {
                panic!("byte slice materialized as a string");
            };
            assert_eq!(s.string_bytes(bytes)?, b"raw bytes");

            let err =
                i64::from_lua("not an int".into_lua(s)?, s).expect_err("string is not an integer");
            assert_eq!(err.message(), "expected integer, got string");

            let range_err =
                u8::from_lua(300_i64.into_lua(s)?, s).expect_err("large integer is not a u8");
            assert_eq!(range_err.message(), "integer out of range for u8");

            let number_range_err =
                u8::from_lua(ScopedValue::Number(300.0), s).expect_err("large number is not a u8");
            assert_eq!(number_range_err.message(), "integer out of range for u8");

            let fractional_err = i64::from_lua(ScopedValue::Number(3.5), s)
                .expect_err("fractional number is not an integer");
            assert_eq!(
                fractional_err.message(),
                "expected integer, got non-integral number 3.5"
            );

            let huge_err = i64::from_lua(ScopedValue::Number(1e19), s)
                .expect_err("huge number is not a Lua integer");
            assert_eq!(
                huge_err.message(),
                "number 10000000000000000000 is out of range for a 64-bit integer"
            );

            let lua_too_large = u64::MAX
                .into_lua(s)
                .expect_err("large u64 cannot fit in a Lua integer");
            assert_eq!(lua_too_large.message(), "integer out of range for Lua: u64");
            Ok(())
        })
        .expect("conversions");
    }

    #[test]
    fn vec_and_hash_map_conversions_use_table_shapes_and_path_errors() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let numbers = vec![1_i32, 2, 3].into_lua(s)?;
            let ScopedValue::Table(number_table) = numbers else {
                panic!("Vec materialized as a table");
            };
            assert_eq!(number_table.len(s)?, 3);
            assert_eq!(number_table.get::<_, i32>(s, 2.0_f64)?, 2);
            let round_trip = Vec::<i32>::from_lua(ScopedValue::Table(number_table), s)?;
            assert_eq!(round_trip, vec![1, 2, 3]);

            let nested = Vec::<Vec<i32>>::from_lua(vec![vec![4_i32], vec![5_i32]].into_lua(s)?, s)?;
            assert_eq!(nested, vec![vec![4], vec![5]]);

            let bad_array = s.create_table()?;
            bad_array.set(s, 1.0_f64, 1_i32)?;
            bad_array.set(s, 2.0_f64, "bad")?;
            let err = Vec::<i32>::from_lua(ScopedValue::Table(bad_array), s)
                .expect_err("bad element type is rejected with a path");
            assert_eq!(err.message(), "at [2]: expected integer, got string");

            let mut map = HashMap::new();
            map.insert("alpha".to_string(), 10_i32);
            map.insert("beta".to_string(), 20_i32);
            let map_value = map.into_lua(s)?;
            let round_trip = HashMap::<String, i32>::from_lua(map_value, s)?;
            assert_eq!(round_trip.get("alpha"), Some(&10));
            assert_eq!(round_trip.get("beta"), Some(&20));

            let bad_map = s.create_table()?;
            bad_map.set(s, 1.0_f64, "bad-key")?;
            let err = HashMap::<String, String>::from_lua(ScopedValue::Table(bad_map), s)
                .expect_err("bad map key type is rejected with a path");
            assert_eq!(
                err.message(),
                "at map pair 1 key: expected string, got number"
            );
            Ok(())
        })
        .expect("table-backed conversions");
    }

    #[test]
    fn table_sequence_helpers_maintain_the_array_border() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let table = s.create_table()?;
            assert_eq!(table.push(s, "alpha")?, 1);
            assert_eq!(table.len(s)?, 1);

            table.set_index(s, 3, "charlie")?;
            assert_eq!(table.len(s)?, 1, "gap keeps the border at one");

            table.set_index(s, 2, "bravo")?;
            assert_eq!(table.len(s)?, 3, "filling the gap absorbs index three");
            assert_eq!(table.get::<_, String>(s, 1.0_f64)?, "alpha");
            assert_eq!(table.get::<_, String>(s, 2.0_f64)?, "bravo");
            assert_eq!(table.get::<_, String>(s, 3.0_f64)?, "charlie");

            let err = table
                .set_index(s, 0, "zero")
                .expect_err("zero is not a sequence index");
            assert_eq!(err.message(), "table sequence index must be positive");
            Ok(())
        })
        .expect("sequence helpers");
    }

    #[test]
    fn result_conversions_propagate_host_errors() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let ok: Result<i32, RuntimeError> = Ok(7);
            assert_eq!(i32::from_lua(ok.into_lua(s)?, s)?, 7);

            let err: Result<i32, RuntimeError> = Err(RuntimeError::runtime("host rejected"));
            let err = err
                .into_lua(s)
                .expect_err("Result::Err propagates through IntoLua");
            assert_eq!(err.message(), "host rejected");

            let ok_multi: Result<(i32, String), RuntimeError> = Ok((3, "done".to_string()));
            let values = ok_multi.into_lua_multi(s)?;
            assert_eq!(values.len(), 2);

            let err_multi: Result<(i32, String), RuntimeError> =
                Err(RuntimeError::runtime("multi rejected"));
            let err = err_multi
                .into_lua_multi(s)
                .expect_err("Result::Err propagates through IntoLuaMulti");
            assert_eq!(err.message(), "multi rejected");
            Ok(())
        })
        .expect("result conversions");
    }

    #[test]
    fn buffer_handles_materialize_and_copy_bytes() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let buffer = s.create_buffer([1, 2, 3, 4])?;
            assert_eq!(buffer.len(s)?, 4);
            assert!(!buffer.is_empty(s)?);
            assert_eq!(s.buffer_bytes(buffer)?, vec![1, 2, 3, 4]);
            let value = buffer.into_lua(s)?;
            assert_eq!(value.type_name(), "buffer");
            let buffer = Buffer::from_lua(value, s)?;
            assert_eq!(buffer.to_vec(s)?, vec![1, 2, 3, 4]);
            Ok(())
        })
        .expect("buffer conversion");
    }

    #[test]
    fn buffer_write_and_fill_are_bounded() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let buffer = s.create_buffer([1, 2, 3, 4])?;

            buffer.write(s, 1, [9, 8])?;
            assert_eq!(buffer.to_vec(s)?, vec![1, 9, 8, 4]);

            let err = buffer
                .write(s, 3, [7, 6])
                .expect_err("write past the fixed buffer end is rejected");
            assert_eq!(err.message(), "buffer write is out of bounds");
            assert_eq!(buffer.to_vec(s)?, vec![1, 9, 8, 4]);

            buffer.fill(s, 5)?;
            assert_eq!(buffer.to_vec(s)?, vec![5, 5, 5, 5]);
            Ok(())
        })
        .expect("buffer mutation");
    }

    #[test]
    fn table_get_set_uses_typed_conversions() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let table = s.create_table()?;
            assert!(table.is_empty(s)?);

            table.set(s, "name", "ruau")?;
            let name: String = table.get(s, "name")?;
            assert_eq!(name, "ruau");

            let missing: Option<String> = table.get(s, "missing")?;
            assert_eq!(missing, None);

            table.set(s, 1.0_f64, "first")?;
            let first: String = table.get(s, 1.0_f64)?;
            assert_eq!(first, "first");
            assert_eq!(table.len(s)?, 1);

            let err = table
                .set(s, f64::NAN, true)
                .expect_err("NaN keys are rejected");
            assert_eq!(err.message(), "table index is NaN");
            Ok(())
        })
        .expect("table conversion");
    }

    #[test]
    fn interned_key_handles_survive_steps_and_collection() {
        let mut vm = crate::test_vm();

        let key = vm
            .step(|s| s.intern_key("score"))
            .expect("intern and root key");
        vm.collect();

        let table = vm
            .step(|s| {
                let table = s.create_table()?;
                table.set_keyed(s, &key, 41_i64)?;
                s.stash_table(table)
            })
            .expect("write retained table through key handle");
        vm.collect();

        vm.step(|s| {
            let table = s.fetch_table(&table)?;

            let before: i64 = table.get_keyed(s, &key)?;
            assert_eq!(before, 41);

            table.set_keyed(s, &key, 42_i64)?;
            let after: i64 = table.get(s, "score")?;
            assert_eq!(after, 42);

            table.set_keyed(s, &key, ())?;
            let missing: Option<i64> = table.get_keyed(s, &key)?;
            assert_eq!(missing, None);
            Ok(())
        })
        .expect("reused key handle after collection");
    }

    #[test]
    fn keyed_cleanup_clears_unretained_entries_without_string_materialization() {
        let mut vm = crate::test_vm();
        let keep = vm.step(|s| s.intern_key("keep")).expect("intern key");
        let also_keep = vm.step(|s| s.intern_key("also_keep")).expect("intern key");
        let table = vm
            .step(|s| {
                let table = s.create_table()?;
                table.set_keyed(s, &keep, 1_i64)?;
                table.set_keyed(s, &also_keep, 2_i64)?;
                table.set(s, "drop", 3_i64)?;
                table.set(s, 1.0_f64, 4_i64)?;
                table.clear_except_keyed(s, [&keep], false)?;
                let dropped: Option<i64> = table.get(s, "drop")?;
                assert_eq!(dropped, None);
                let array_value: i64 = table.get(s, 1.0_f64)?;
                assert_eq!(array_value, 4, "non-string keys can be preserved");

                table.clear_except_keyed(s, [&keep], true)?;
                let other: Option<i64> = table.get_keyed(s, &also_keep)?;
                assert_eq!(other, None);
                let array_value: Option<i64> = table.get(s, 1.0_f64)?;
                assert_eq!(array_value, None, "non-string keys can be cleared");
                s.stash_table(table)
            })
            .expect("cleanup");
        vm.collect();
        vm.step(|s| {
            let table = s.fetch_table(&table)?;
            let kept: i64 = table.get_keyed(s, &keep)?;
            assert_eq!(kept, 1);
            Ok(())
        })
        .expect("kept entry survives cleanup and collection");
    }

    #[test]
    fn host_freeze_blocks_script_writes_but_not_host_writes() {
        let mut vm = runtime_compile_test_vm();
        vm.step(|s| {
            let table = s.create_table()?;
            table.set(s, "x", 1_i64)?;
            table.freeze(s)?;
            table.freeze(s)?;
            assert!(table.is_frozen(s)?);

            let globals = s.thread.borrow().globals.expect("globals");
            let mut heap = s.heap.borrow_mut();
            let key = heap.intern_str(b"obs").expect("intern global name");
            heap.table_mut(globals)
                .expect("globals table")
                .set(RawValue::String(key), RawValue::Table(table.raw()));
            drop(heap);

            let results = s
                .eval_chunk(b"return pcall(function() obs.x = 2 end)", b"=freeze")?
                .into_vec();
            assert!(
                matches!(results.as_slice(), [ScopedValue::Boolean(false), ..]),
                "script writes to frozen host tables must raise"
            );

            table.set(s, "x", 3_i64)?;
            assert_eq!(table.get::<_, i64>(s, "x")?, 3);
            Ok(())
        })
        .expect("host freeze");
    }

    #[test]
    fn host_freeze_deep_marks_reachable_tables() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let root = s.create_table()?;
            let child = s.create_table()?;
            child.set(s, "value", 1_i64)?;
            root.set(s, "child", child)?;
            root.set(s, "self", root)?;

            root.freeze_deep(s)?;
            assert!(root.is_frozen(s)?);
            assert!(child.is_frozen(s)?);

            child.set(s, "value", 2_i64)?;
            assert_eq!(child.get::<_, i64>(s, "value")?, 2);
            Ok(())
        })
        .expect("deep freeze");
    }

    #[test]
    fn interned_key_handles_reject_cross_vm_use() {
        let mut first = crate::test_vm();
        let key = first
            .step(|s| s.intern_key("score"))
            .expect("intern key in first VM");

        let mut second = crate::test_vm();
        second
            .step(|s| {
                let table = s.create_table()?;
                let error = table
                    .set_keyed(s, &key, 1_i64)
                    .expect_err("key handle belongs to a different VM");
                assert_eq!(
                    error.message(),
                    "key handle no longer resolves: cross-VM host registry pin"
                );
                Ok(())
            })
            .expect("cross-VM key handle rejection is catchable");
    }

    #[test]
    fn table_pairs_snapshot_live_entries_as_branded_values() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let table = s.create_table()?;
            table.set(s, 1.0_f64, "first")?;
            table.set(s, "name", "ruau")?;
            table.set(s, "gone", "stale")?;
            table.set(s, "gone", ())?;

            let mut seen = Vec::new();
            for (key, value) in table.pairs(s)? {
                let value = String::from_lua(value, s)?;
                match key {
                    ScopedValue::Number(index) => seen.push((format!("#{index}"), value)),
                    ScopedValue::String(key) => {
                        seen.push((String::from_lua(ScopedValue::String(key), s)?, value))
                    }
                    other => panic!("unexpected key type: {other:?}"),
                }
            }
            seen.sort();

            assert_eq!(
                seen,
                vec![
                    ("#1".to_string(), "first".to_string()),
                    ("name".to_string(), "ruau".to_string()),
                ]
            );
            Ok(())
        })
        .expect("table pairs");
    }

    #[test]
    fn table_metatable_access_is_host_side_and_raw() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let table = s.create_table()?;
            assert!(table.metatable(s)?.is_none());

            let metatable = s.create_table()?;
            metatable.set(s, "__marker", true)?;
            metatable.set(s, "__metatable", "locked")?;
            table.set_metatable(s, Some(metatable))?;

            let observed = table.metatable(s)?.expect("metatable is installed");
            let marker: bool = observed.get(s, "__marker")?;
            assert!(marker);

            table.set_metatable(s, None)?;
            assert!(table.metatable(s)?.is_none());
            Ok(())
        })
        .expect("table metatable access");
    }

    #[test]
    fn tuple_multivalue_conversions_preserve_arity_and_types() {
        let mut vm = crate::test_vm();
        vm.step(|s| {
            let values = (true, 99_i64, "done").into_lua_multi(s)?;
            assert_eq!(values.len(), 3);

            let tuple = <(bool, i64, String) as FromLuaMulti>::from_lua_multi(values, s)?;
            assert_eq!(tuple, (true, 99, "done".to_string()));

            let wrong_arity = MultiValue::from_values(vec![true.into_lua(s)?]);
            let err = <(bool, i64) as FromLuaMulti>::from_lua_multi(wrong_arity, s)
                .expect_err("wrong arity is rejected");
            assert_eq!(err.message(), "expected 2 Lua values, got 1");
            Ok(())
        })
        .expect("multi conversion");
    }

    #[test]
    fn step_returns_an_owned_scalar() {
        let mut vm = crate::test_vm();
        let n: i64 = vm.step(|_s| Ok(42)).expect("step");
        assert_eq!(n, 42);
        // A unit step is also valid.
        vm.step(|_s| Ok(())).expect("unit step");
    }

    #[test]
    fn sequential_steps_reset_the_reentry_guard() {
        let mut vm = crate::test_vm();
        vm.step(|_s| Ok(())).expect("first step");
        // The guard cleared on the first step's drop, so a second step opens.
        vm.step(|_s| Ok(())).expect("second step");
        // An errored step also clears the guard (the scope still drops).
        assert!(
            vm.step(|_s| Err::<(), _>(RuntimeError::runtime("boom")))
                .is_err()
        );
        vm.step(|_s| Ok(())).expect("step after an errored step");
    }

    /// Compiles a trusted root chunk for the eval/load tests.
    fn compile_root(source: &str) -> ruau_bytecode::BytecodeChunk {
        ruau_bytecode::compile_source(source, &ruau_bytecode::CompileOptions::default(), None)
            .expect("root chunk compiles")
    }

    fn runtime_compile_test_vm() -> crate::Vm {
        crate::Vm::builder()
            .runtime_capabilities(
                crate::RuntimeCapabilities::default().enable_runtime_compilation(),
            )
            .build_for_test()
    }

    /// Installs a scoped host function as a global, the test-only counterpart
    /// of a native-module binding.
    fn install_scoped_host(vm: &mut crate::Vm, name: &[u8], f: Box<dyn crate::ScopedHostFunction>) {
        let globals = vm.main_thread().unwrap().globals.expect("globals");
        let closure = vm
            .heap_mut()
            .alloc_scoped_host(f)
            .expect("alloc scoped host");
        let key = vm.heap_mut().intern_str(name).expect("intern name");
        vm.heap_mut()
            .table_mut(globals)
            .expect("globals table")
            .set(RawValue::String(key), RawValue::Function(closure));
    }

    /// A host function that evaluates its string argument as a chunk and
    /// returns the chunk's first result as a number.
    fn evalhost(scope: &Scope<'_>, source: &str) -> Result<f64, RuntimeError> {
        let results = scope.eval_chunk(source.as_bytes(), b"=nested")?;
        let value = results.iter().next().unwrap_or(ScopedValue::Nil);
        f64::from_lua(value, scope)
    }

    /// On a sandboxed VM the eval'd chunk shares the calling chunk's global
    /// environment (the `sandbox_thread` proxy) in both directions: it reads a
    /// global the root chunk set, and the root sees a global it set.
    #[test]
    fn eval_chunk_shares_the_calling_chunks_environment_on_a_sandboxed_vm() {
        let mut vm = crate::Vm::builder()
            .ambient(crate::Ambient::deterministic(0))
            .limits(crate::Limits::unlimited())
            .runtime_capabilities(
                crate::RuntimeCapabilities::default().enable_runtime_compilation(),
            )
            .sandboxed()
            .build()
            .expect("sandboxed vm builds");
        let module = vm
            .load_named(&compile_root("G = 41"), b"=root")
            .expect("load root");
        vm.call(&module, Default::default()).expect("root sets G");

        let result = vm
            .step(|s| {
                let results = s.eval_chunk(b"G = G + 1\nH = 5\nreturn G", b"=eval")?;
                match results.iter().next() {
                    Some(ScopedValue::Number(value)) => Ok(value),
                    other => Err(RuntimeError::runtime(format!(
                        "expected a number result, got {other:?}"
                    ))),
                }
            })
            .expect("eval_chunk reads and writes the shared environment");
        assert!(
            (result - 42.0).abs() < f64::EPSILON,
            "the eval'd chunk reads the root chunk's G: {result}"
        );

        // ...and the root environment sees the eval'd chunk's write.
        let module = vm
            .load_named(&compile_root("return H"), b"=root2")
            .expect("load root2");
        match vm
            .call(&module, Default::default())
            .expect("root reads H")
            .as_slice()
        {
            [RawValue::Number(h)] => assert!((h - 5.0).abs() < f64::EPSILON),
            other => panic!("expected the eval'd chunk's H, got {other:?}"),
        }
    }

    /// `load_chunk` compiles without running: the chunk's side effects land
    /// only when the host calls the returned function.
    #[test]
    fn load_chunk_defers_execution_until_called() {
        let mut vm = runtime_compile_test_vm();
        vm.step(|s| {
            let function = s.load_chunk(b"SIDE = 7\nreturn 3", b"=deferred")?;
            assert!(
                s.global_value(b"SIDE").is_none(),
                "loading must not run the chunk"
            );
            let result: f64 = s.call(function, ())?;
            assert!((result - 3.0).abs() < f64::EPSILON);
            assert!(
                matches!(s.global_value(b"SIDE"), Some(ScopedValue::Number(n)) if n == 7.0),
                "calling the loaded chunk runs its body"
            );
            Ok(())
        })
        .expect("load_chunk defers execution");
    }

    /// A chunk can evaluate another chunk through a host function: the host
    /// re-enters the VM with `eval_chunk` mid-script, twice nested, and each
    /// eval'd chunk sees the same environment (including the host binding).
    #[test]
    fn eval_chunk_nests_through_a_host_function() {
        let mut vm = runtime_compile_test_vm();
        install_scoped_host(
            &mut vm,
            b"evalhost",
            crate::scoped_host_fn(|scope: &Scope<'_>, source: String| evalhost(scope, &source)),
        );
        let module = vm
            .load_named(
                &compile_root(r#"return evalhost("return evalhost('return 7') + 1") + 1"#),
                b"=root",
            )
            .expect("load root");
        match vm
            .call(&module, Default::default())
            .expect("nested eval runs")
            .as_slice()
        {
            [RawValue::Number(value)] => assert!((value - 9.0).abs() < f64::EPSILON),
            other => panic!("expected the doubly-nested result, got {other:?}"),
        }
    }

    /// Gas exhaustion inside an eval'd chunk unwinds through the host frame and
    /// the root chunk cleanly: the entry fails with the shared in-flight budget
    /// spent, and the VM stays usable afterwards.
    #[test]
    fn gas_exhaustion_inside_an_eval_chunk_unwinds_cleanly() {
        let mut vm = runtime_compile_test_vm();
        install_scoped_host(
            &mut vm,
            b"evalhost",
            crate::scoped_host_fn(|scope: &Scope<'_>, source: String| evalhost(scope, &source)),
        );
        let module = vm
            .load_named(&compile_root("evalhost('while true do end')"), b"=root")
            .expect("load root");
        let error = vm
            .call(
                &module,
                crate::CallOptions::new().limits(crate::Limits {
                    gas: Some(10_000),
                    ..crate::Limits::unlimited()
                }),
            )
            .expect_err("the eval'd loop exhausts the entry's gas budget");
        assert_eq!(
            error.kind,
            RuntimeErrorKind::Runtime,
            "gas exhaustion is the ordinary catchable runtime kind"
        );
        // The unwind left the VM healthy: a later default-limit entry runs.
        let module = vm
            .load_named(&compile_root("return 1"), b"=after")
            .expect("load after");
        assert_eq!(
            vm.call(&module, Default::default())
                .expect("the VM stays usable")
                .len(),
            1
        );
    }

    /// A compile error surfaces as a catchable `RuntimeError` carrying the
    /// chunk name and the failing line, in `loadstring`'s location shape.
    #[test]
    fn eval_chunk_compile_error_carries_chunk_name_and_line() {
        let mut vm = runtime_compile_test_vm();
        vm.step(|s| {
            let error = s
                .eval_chunk(b"\n\nlocal = 3", b"=mychunk")
                .expect_err("malformed source fails to compile");
            assert_eq!(error.kind(), RuntimeErrorKind::Runtime);
            assert!(
                error.message().starts_with("mychunk:3:"),
                "the compile error names the chunk and line: {:?}",
                error.message()
            );
            // `load_chunk` reports the same shape without running anything.
            let error = s
                .load_chunk(b"return +", b"=other")
                .expect_err("malformed source fails to load");
            assert!(
                error.message().starts_with("other:1:"),
                "the load error names the chunk and line: {:?}",
                error.message()
            );
            Ok(())
        })
        .expect("compile errors are catchable");
    }

    /// With runtime compilation disabled, `load_chunk`/`eval_chunk` fail
    /// closed with a clear catchable error.
    #[test]
    fn eval_chunk_fails_closed_when_runtime_compilation_is_gated_off() {
        let mut vm = crate::Vm::builder()
            .runtime_capabilities(crate::RuntimeCapabilities::default())
            .build_for_test();
        vm.step(|s| {
            let error = s
                .eval_chunk(b"return 1", b"=gated")
                .expect_err("the gate fails closed");
            assert_eq!(error.kind(), RuntimeErrorKind::Runtime);
            assert!(
                error.message().contains("runtime compilation is disabled"),
                "the gate error is self-describing: {:?}",
                error.message()
            );
            let error = s
                .load_chunk(b"return 1", b"=gated")
                .expect_err("load_chunk is gated identically");
            assert!(error.message().contains("runtime compilation is disabled"));
            Ok(())
        })
        .expect("the gate error is catchable, not fatal");
    }

    /// The `Limits` runtime-compilation caps apply to `eval_chunk` exactly as
    /// to `loadstring`, and the cap diagnostic names the chunk.
    #[test]
    fn eval_chunk_honors_runtime_compile_caps_from_limits() {
        let mut vm = crate::Vm::builder()
            .limits(crate::Limits {
                max_runtime_compile_source_bytes: Some(8),
                ..crate::Limits::unlimited()
            })
            .runtime_capabilities(
                crate::RuntimeCapabilities::default().enable_runtime_compilation(),
            )
            .build_for_test();
        vm.step(|s| {
            let error = s
                .eval_chunk(b"return 1 + 2 + 3 + 4", b"=capped")
                .expect_err("an over-cap source is rejected");
            assert_eq!(error.kind(), RuntimeErrorKind::Runtime);
            assert!(
                error
                    .message()
                    .starts_with("capped: runtime compilation source byte limit exceeded"),
                "the cap error names the chunk and the cap: {:?}",
                error.message()
            );
            Ok(())
        })
        .expect("the cap error is catchable, not fatal");
    }

    /// A disabled library's constants are not folded into an eval'd chunk,
    /// even when the chunk escalates the optimization level with a hot
    /// comment — the `RuntimeCapabilities::compile_source` suppression rule applies to the
    /// VM-local runtime compiler too. With the library enabled, the same chunk folds
    /// (or reads) the constant normally.
    #[test]
    fn eval_chunk_suppresses_disabled_library_folds() {
        const PROBE: &[u8] =
            b"--!optimize 2\nlocal ok, value = pcall(function() return math.pi end)\nreturn ok, value";

        fn probe_math_pi(vm: &mut crate::Vm) -> (bool, Option<f64>) {
            vm.step(|s| {
                let results = s.eval_chunk(PROBE, b"=folds")?.into_vec();
                match results.as_slice() {
                    [ScopedValue::Boolean(ok), value] => {
                        let value = match value {
                            ScopedValue::Number(n) => Some(*n),
                            _ => None,
                        };
                        Ok((*ok, value))
                    }
                    other => Err(RuntimeError::runtime(format!(
                        "expected pcall results, got {other:?}"
                    ))),
                }
            })
            .expect("the probe chunk evaluates")
        }

        // `math` omitted: the reference must reach the absent runtime
        // global and fail closed, not a compile-time folded constant.
        let mut vm = crate::Vm::builder()
            .runtime_capabilities(
                crate::RuntimeCapabilities::from_libraries(
                    crate::Library::ALL
                        .into_iter()
                        .filter(|library| *library != crate::Library::Math),
                )
                .enable_runtime_compilation(),
            )
            .build_for_test();
        let (ok, value) = probe_math_pi(&mut vm);
        assert!(
            !ok && value.is_none(),
            "math.pi must not be folded into the eval'd chunk: ok={ok}, value={value:?}"
        );

        // `math` enabled: the same chunk reads the constant normally.
        let mut vm = runtime_compile_test_vm();
        let (ok, value) = probe_math_pi(&mut vm);
        assert!(ok, "the full capability set serves math.pi");
        assert!(
            value.is_some_and(|pi| (pi - std::f64::consts::PI).abs() < 1e-12),
            "math.pi has its ordinary value: {value:?}"
        );
    }
}
