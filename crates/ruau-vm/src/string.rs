//! Interned strings.
//!
//! Strings are immutable, interned per VM, and identified by handle (port
//! `lstring.cpp`). The string objects live in the heap's string arena; the
//! [`StringInterner`] maps content to the handle so equal byte sequences share
//! one object. The interner's map and every table's key index hash through the
//! VM's deterministic keyed hasher, seeded from `AmbientConfig::hash_seed`, so
//! each VM has an independent collision profile while tests remain replayable.

use std::collections::{HashMap, hash_map::Entry};
#[cfg(any())]
use std::hash::BuildHasher;

use ruau_vm_api::{RawGc, marker};

use crate::hash::VmBuildHasher;

/// An interned string object: immutable bytes. Table-key placement hashes by the
/// string's arena handle, not its content, so the object caches no content hash.
pub struct InternedString {
    bytes: Box<[u8]>,
}

impl InternedString {
    /// Builds a string object from raw bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Box<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    /// The raw bytes (Luau strings are byte strings, not required to be UTF-8).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The number of bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the string is empty.
    #[cfg(any(test, feature = "conformance"))]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// GC: the metered byte footprint to release when this string is swept — the
    /// content bytes charged at intern time. The interner releases its own key/entry
    /// charge separately, via [`StringInterner::remove`].
    pub(crate) fn gc_footprint(&self) -> usize {
        self.bytes.len()
    }

    /// A UTF-8 view, if the bytes are valid UTF-8.
    #[cfg(any(test, feature = "conformance"))]
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }
}

/// The content-to-handle map backing interning. The string objects themselves
/// live in the heap's string arena; this only records which content already has
/// a handle.
#[derive(Default)]
pub struct StringInterner {
    map: HashMap<Box<[u8]>, RawGc<marker::Str>, VmBuildHasher>,
    /// The heap's memory meter, charged for each key copy this map owns so a
    /// flood of distinct interned strings counts against the per-VM cap.
    meter: crate::heap::MemoryMeter,
}

impl StringInterner {
    /// An empty interner charging the heap's shared meter and using the heap's
    /// per-VM keyed hash builder.
    #[must_use]
    pub(crate) fn with_meter_and_hash(
        meter: crate::heap::MemoryMeter,
        hash_builder: VmBuildHasher,
    ) -> Self {
        Self {
            map: HashMap::with_hasher(hash_builder),
            meter,
        }
    }

    /// The handle for `bytes` if it has already been interned.
    #[must_use]
    pub fn lookup(&self, bytes: &[u8]) -> Option<RawGc<marker::Str>> {
        self.map.get(bytes).copied()
    }

    /// Records a freshly allocated handle for `bytes`, returning the handle that
    /// ends up stored (an existing one wins a race-free re-check).
    pub fn insert(&mut self, bytes: &[u8], handle: RawGc<marker::Str>) -> RawGc<marker::Str> {
        match self.map.entry(bytes.into()) {
            Entry::Occupied(existing) => *existing.get(),
            Entry::Vacant(slot) => {
                // The key is an owned copy of the content plus one map slot; charge
                // both so a flood of distinct strings counts against the cap.
                let added = bytes.len() + std::mem::size_of::<(Box<[u8]>, RawGc<marker::Str>)>();
                self.meter.charge(added);
                *slot.insert(handle)
            }
        }
    }

    /// Drops the interner entry for `bytes` — called when its `InternedString` is swept by the
    /// collector — and releases the key copy's metered charge, so a later intern of the
    /// same content allocates a fresh handle (the old one is already generation-stale)
    /// and re-counts a fresh entry. This is the weak half of the interner: the map
    /// records identity, it does not keep a string alive.
    pub fn remove(&mut self, bytes: &[u8]) {
        if self.map.remove(bytes).is_some() {
            let freed = bytes.len() + std::mem::size_of::<(Box<[u8]>, RawGc<marker::Str>)>();
            self.meter.adjust(freed, 0);
        }
    }

    #[cfg(any())]
    pub(crate) fn hash_for(&self, bytes: &[u8]) -> u64 {
        self.map.hasher().hash_one(bytes)
    }
}
