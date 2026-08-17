//! The Lua table object: an array part plus a hash part (port `ltable.cpp`).
//!
//! The array part holds dense positive-integer keys; everything else lives in
//! the hash part. Keys are normalized (an integer-valued float becomes an
//! integer; `nil` and `NaN` keys are rejected), matching Luau key semantics.
//!
//! The hash part is an insertion-ordered slot vector with a key index, so `next`
//! enumerates every
//! live entry exactly once and a field may be cleared mid-traversal (Lua allows
//! assigning `nil` to an existing key while iterating). The key index hashes with
//! the VM's per-VM keyed hasher; iteration order does not depend on it (it walks
//! the insertion-ordered slot vector). Exact upstream iteration order and the
//! histogram-based resize are deferred to the performance work; the array absorbs
//! appended dense keys, which keeps the representation correct.

use std::collections::TryReserveError;
#[cfg(any())]
use std::hash::BuildHasher;

use crate::{
    api::{RawGc, RawValue, marker},
    hash::VmBuildHasher,
    heap::MemoryMeter,
};

/// A normalized, hashable table key. Handle keys compare by arena identity
/// (index and generation); an integer-valued number normalizes to the array
/// index, while a native integer is a distinct key — `t[1.0]` and `t[1]` do not
/// collide in this revision.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum LuaKey {
    Boolean(bool),
    /// An integer-valued *number* (`f64`): the array-part index. A `Number(1.0)`
    /// key lives here, distinct from a native-integer `1`.
    Integer(i64),
    /// A native 64-bit integer key (`RawValue::Integer`), distinct from the
    /// integer-valued number above — the revision keys `t[1]` and `t[1.0]`
    /// separately.
    NativeInt(i64),
    /// Non-integer-valued number, by canonical `f64` bits.
    Number(u64),
    /// Three-lane vector, by `f32` bits.
    Vector([u32; 3]),
    /// Opaque host token.
    LightUserdata {
        handle: u32,
        tag: u8,
    },
    /// A heap handle key, tagged by kind so two kinds never collide.
    Handle {
        kind: HandleKind,
        index: u32,
        generation: u32,
    },
}

/// Why a value cannot be used as a table key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRejection {
    Nil,
    NaN,
}

impl KeyRejection {
    #[must_use]
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Nil => "table index is nil",
            Self::NaN => "table index is NaN",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum HandleKind {
    Str,
    Table,
    Function,
    Userdata,
    Thread,
    Buffer,
}

/// Normalizes a value into a table key, or `None` for the rejected keys (`nil`
/// and `NaN`).
fn normalize_key(value: RawValue) -> Option<LuaKey> {
    if key_rejection(value).is_some() {
        return None;
    }
    match value {
        RawValue::Nil => None,
        RawValue::Boolean(b) => Some(LuaKey::Boolean(b)),
        RawValue::Integer(i) => Some(LuaKey::NativeInt(i)),
        RawValue::Number(n) => {
            // An integer-valued float keys as the integer it equals.
            if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
                Some(LuaKey::Integer(n as i64))
            } else {
                // Canonicalize -0.0 to 0.0 so they are one key.
                let bits = if n == 0.0 {
                    0.0_f64.to_bits()
                } else {
                    n.to_bits()
                };
                Some(LuaKey::Number(bits))
            }
        }
        RawValue::Vector(v) => {
            // Canonicalize -0.0 (single-precision `0x80000000`) to +0.0 per
            // component so a key with a negative-zero component hashes and
            // compares equal to its positive counterpart, mirroring upstream
            // `hashvec` (`ltable.cpp`). For every other non-NaN float, equal
            // values share one bit pattern, so bit equality matches `luai_veceq`.
            let canon = |component: f32| {
                let bits = component.to_bits();
                if bits == 0x8000_0000 { 0 } else { bits }
            };
            Some(LuaKey::Vector([canon(v[0]), canon(v[1]), canon(v[2])]))
        }
        RawValue::LightUserdata { handle, tag } => Some(LuaKey::LightUserdata { handle, tag }),
        RawValue::String(g) => Some(handle_key(HandleKind::Str, g.index(), g.generation())),
        RawValue::Table(g) => Some(handle_key(HandleKind::Table, g.index(), g.generation())),
        RawValue::Function(g) => Some(handle_key(HandleKind::Function, g.index(), g.generation())),
        RawValue::Userdata(g) => Some(handle_key(HandleKind::Userdata, g.index(), g.generation())),
        RawValue::Thread(g) => Some(handle_key(HandleKind::Thread, g.index(), g.generation())),
        RawValue::Buffer(g) => Some(handle_key(HandleKind::Buffer, g.index(), g.generation())),
    }
}

/// Returns the reason `value` cannot be a table key, if any.
#[must_use]
pub fn key_rejection(value: RawValue) -> Option<KeyRejection> {
    match value {
        RawValue::Nil => Some(KeyRejection::Nil),
        RawValue::Number(n) if n.is_nan() => Some(KeyRejection::NaN),
        RawValue::Vector(v) if v.iter().any(|c| c.is_nan()) => Some(KeyRejection::NaN),
        _ => None,
    }
}

fn handle_key(kind: HandleKind, index: u32, generation: u32) -> LuaKey {
    LuaKey::Handle {
        kind,
        index,
        generation,
    }
}

/// The integer-valued-number key of a 0-based array slot: slot `i` is logical key
/// `i + 1`. Array slots are bounded well below `2^53`, so the cast is exact.
#[allow(clippy::cast_precision_loss)]
fn array_key(slot: usize) -> f64 {
    (slot + 1) as f64
}

/// One slot in the hash part. A `None` value is a tombstone: the slot keeps its
/// position so `next` indices stay stable, but the entry is logically absent.
/// `key_value` is the original key (what `next` hands back); the normalized key
/// lives in the index map.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct HashSlot {
    key_value: RawValue,
    value: Option<RawValue>,
}

/// A Lua table.
pub struct LuaTable {
    /// Dense positive-integer keys; `array[i]` is logical key `i + 1`.
    array: Vec<RawValue>,
    /// Everything else, insertion-ordered for stable `next`.
    hash: Vec<HashSlot>,
    index: std::collections::HashMap<LuaKey, usize, VmBuildHasher>,
    /// The table's metatable, if any. `__index`, `__newindex`, and the
    /// operator metamethods are looked up here.
    metatable: Option<RawGc<marker::Table>>,
    /// When set, writes raise rather than mutate (the `readonly` sandbox flag).
    pub readonly: bool,
    /// Marks an environment table frozen by `safeenv`.
    pub safeenv: bool,
    /// The heap's memory meter and this table's last-charged footprint, so the
    /// array/hash/index growth counts against the per-VM cap. An orphan meter
    /// until the table is allocated into a heap ([`attach_meter`](Self::attach_meter)).
    meter: MemoryMeter,
    charged: usize,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct LuaTableImage {
    pub array: Vec<RawValue>,
    pub hash: Vec<HashSlotImage>,
    pub metatable: Option<RawGc<marker::Table>>,
    pub readonly: bool,
    pub safeenv: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct HashSlotImage {
    pub key_value: RawValue,
    pub value: Option<RawValue>,
}

impl LuaTable {
    /// An empty table charging an orphan meter until allocated into a heap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_hash_builder(VmBuildHasher::default())
    }

    fn with_hash_builder(hash_builder: VmBuildHasher) -> Self {
        Self {
            array: Vec::new(),
            hash: Vec::new(),
            index: std::collections::HashMap::with_hasher(hash_builder),
            metatable: None,
            readonly: false,
            safeenv: false,
            meter: MemoryMeter::default(),
            charged: 0,
        }
    }

    /// A table whose array part is pre-filled with `array` — `table.create`'s
    /// sizing, so a later write into a slot lands in the array (and `#t` reflects
    /// the pre-sized length once a non-`nil` border exists), matching upstream
    /// `lua_createtable`'s array hint.
    #[must_use]
    pub fn with_array(array: Vec<RawValue>) -> Self {
        Self {
            array,
            ..Self::new()
        }
    }

    /// A table with reserved array capacity, for bytecode table-shape hints.
    ///
    /// # Errors
    /// Returns [`TryReserveError`] if reserving the array capacity fails.
    pub(crate) fn try_with_array_capacity(capacity: usize) -> Result<Self, TryReserveError> {
        let mut array = Vec::new();
        array.try_reserve_exact(capacity)?;
        Ok(Self {
            array,
            ..Self::new()
        })
    }

    /// Byte footprint for an array capacity hint, or `None` on overflow.
    #[must_use]
    pub(crate) fn array_capacity_footprint(capacity: usize) -> Option<usize> {
        capacity.checked_mul(std::mem::size_of::<RawValue>())
    }

    /// Points this table's containers at the heap's shared meter and charges
    /// their current footprint — called when the table is allocated into the heap.
    /// A clean hand-off: it releases its charge from the previous meter first, so
    /// a second attach (or a re-home) never double-counts or strands a charge.
    pub(crate) fn attach_meter(&mut self, meter: MemoryMeter) {
        self.meter.adjust(self.charged, 0);
        self.meter = meter;
        self.charged = 0;
        self.recharge();
    }

    /// Re-hashes the key index with the heap's per-VM hash builder. Tables are
    /// often constructed as orphan values and allocated later; allocation calls
    /// this so every resident table uses the owning VM's seed.
    pub(crate) fn attach_hash_builder(&mut self, hash_builder: VmBuildHasher) {
        if self.index.hasher().seed() == hash_builder.seed() {
            return;
        }
        let mut index =
            std::collections::HashMap::with_capacity_and_hasher(self.index.len(), hash_builder);
        for (&key, &slot) in &self.index {
            index.insert(key, slot);
        }
        self.index = index;
        self.recharge();
    }

    /// The byte footprint of this table's containers (reserved capacity) —
    /// what a shallow copy of it (`table.clone`) or a full pair snapshot
    /// (`table.foreach`) will allocate, so the caller can pre-charge it
    /// against the memory cap before the bulk alloc.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.array.capacity() * std::mem::size_of::<RawValue>()
            + self.hash.capacity() * std::mem::size_of::<HashSlot>()
            + self.index.capacity() * std::mem::size_of::<(LuaKey, usize)>()
    }

    /// Reconciles the meter with the table's current footprint after a mutation.
    fn recharge(&mut self) {
        let now = self.footprint();
        if now == self.charged {
            // In-place writes don't change container capacities — skip the
            // meter round-trip on the common path.
            return;
        }
        self.meter.adjust(self.charged, now);
        self.charged = now;
    }

    /// This table's metatable, if any.
    #[must_use]
    pub fn metatable(&self) -> Option<RawGc<marker::Table>> {
        self.metatable
    }

    /// Sets (or clears, with `None`) this table's metatable.
    pub fn set_metatable(&mut self, metatable: Option<RawGc<marker::Table>>) {
        self.metatable = metatable;
    }

    /// GC: the metered byte footprint to release when this table is swept (its
    /// array, hash, and index capacity charged to the heap meter).
    pub(crate) fn gc_footprint(&self) -> usize {
        self.charged
    }

    /// GC: appends this table's outgoing handles — array and hash entries (keys
    /// and values) and the metatable — to the collector's work list, reserving
    /// fallibly so a very wide table cannot abort the process mid-collection.
    pub(crate) fn gc_trace<V: crate::gc::GcVisit>(
        &self,
        v: &mut V,
    ) -> Result<(), crate::gc::GcAbort> {
        use crate::gc::GcRef;
        for value in &self.array {
            if let Some((child, generation)) = GcRef::from_value_gen(*value) {
                v.visit(child, generation)?;
            }
        }
        for slot in &self.hash {
            // Only a live entry (value present) roots its key and value. A tombstone
            // (`value: None`, kept for stable `next` ordering) is a removed entry, so its
            // `key_value` must not keep the key alive — and after a weak clear it would
            // otherwise dangle.
            if let Some(value) = slot.value {
                if let Some((child, generation)) = GcRef::from_value_gen(slot.key_value) {
                    v.visit(child, generation)?;
                }
                if let Some((child, generation)) = GcRef::from_value_gen(value) {
                    v.visit(child, generation)?;
                }
            }
        }
        if let Some(metatable) = self.metatable {
            v.visit(GcRef::Table(metatable.index()), metatable.generation())?;
        }
        Ok(())
    }

    /// GC: like [`gc_trace`](Self::gc_trace) but for a weak table — the component(s) the
    /// `__mode` declares weak are *not* traced, so an entry referenced only through this
    /// table stays white and is cleared in the collector's atomic phase. Array keys are
    /// integers (never handles), so only `weak_values` affects the array part. The
    /// metatable is always strong.
    ///
    /// A string is the one exception: Luau treats strings as values that are never weak
    /// (`isobjcleared` `stringmark`s them, lgc.cpp:608-619), so a string component is
    /// always traced — it survives and is never cleared even when its side is weak.
    pub(crate) fn gc_trace_weak(
        &self,
        out: &mut Vec<crate::gc::GcRef>,
        weak_keys: bool,
        weak_values: bool,
    ) -> Result<(), crate::gc::GcAbort> {
        use crate::gc::{GcRef, try_push};
        // Trace `value` unless it is weakly held *and* collectable-as-weak — a string is
        // never weak, so it is always traced.
        let strong = |value: RawValue, weak: bool| !weak || matches!(value, RawValue::String(_));
        for &value in &self.array {
            if strong(value, weak_values)
                && let Some(child) = GcRef::from_value(value)
            {
                try_push(out, child)?;
            }
        }
        for slot in &self.hash {
            // Only a live entry roots anything (a `value: None` tombstone is removed).
            if let Some(value) = slot.value {
                if strong(slot.key_value, weak_keys)
                    && let Some(child) = GcRef::from_value(slot.key_value)
                {
                    try_push(out, child)?;
                }
                if strong(value, weak_values)
                    && let Some(child) = GcRef::from_value(value)
                {
                    try_push(out, child)?;
                }
            }
        }
        if let Some(metatable) = self.metatable {
            try_push(out, GcRef::Table(metatable.index()))?;
        }
        Ok(())
    }

    /// GC: visits every live `(key, value)` entry — array (key is the integer index as a
    /// number) then hash — for the weak-table clear pass to test each entry's weak
    /// component. Skips `nil` array holes and tombstoned hash slots.
    pub(crate) fn for_each_entry(&self, mut visit: impl FnMut(RawValue, RawValue)) {
        for (i, &value) in self.array.iter().enumerate() {
            if !matches!(value, RawValue::Nil) {
                visit(RawValue::Number((i + 1) as f64), value);
            }
        }
        for slot in &self.hash {
            if let Some(value) = slot.value {
                visit(slot.key_value, value);
            }
        }
    }

    /// Reads `key`, returning `nil` for any absent key.
    #[must_use]
    pub fn get(&self, key: RawValue) -> RawValue {
        let Some(norm) = normalize_key(key) else {
            return RawValue::Nil;
        };
        if let LuaKey::Integer(i) = norm
            && let Some(slot) = self.array_slot(i)
        {
            return self.array[slot];
        }
        match self.index.get(&norm) {
            Some(&idx) => self.hash[idx].value.unwrap_or(RawValue::Nil),
            None => RawValue::Nil,
        }
    }

    /// Writes `key = value`. Returns whether the write was applied; a `nil` or
    /// `NaN` key is rejected (`false`). Reconciles the memory meter after any
    /// growth so a table inflating past the per-VM cap is caught at the safepoint.
    pub fn set(&mut self, key: RawValue, value: RawValue) -> bool {
        let applied = self.set_inner(key, value);
        self.recharge();
        applied
    }

    fn set_inner(&mut self, key: RawValue, value: RawValue) -> bool {
        let Some(norm) = normalize_key(key) else {
            return false;
        };
        if let LuaKey::Integer(i) = norm {
            if let Some(slot) = self.array_slot(i) {
                self.array[slot] = value;
                return true;
            }
            // Appending exactly past the end grows the array, then absorbs any
            // contiguous successors already sitting in the hash part.
            if i >= 1
                && (i as u64) == self.array.len() as u64 + 1
                && !matches!(value, RawValue::Nil)
            {
                self.array.push(value);
                self.absorb_from_hash();
                return true;
            }
        }
        self.hash_set(norm, key, value);
        true
    }

    fn array_slot(&self, i: i64) -> Option<usize> {
        if i >= 1 && (i as u64) <= self.array.len() as u64 {
            Some((i - 1) as usize)
        } else {
            None
        }
    }

    fn hash_set(&mut self, norm: LuaKey, key_value: RawValue, value: RawValue) {
        match self.index.get(&norm).copied() {
            Some(idx) => {
                self.hash[idx].value = if matches!(value, RawValue::Nil) {
                    None
                } else {
                    Some(value)
                };
            }
            None => {
                if matches!(value, RawValue::Nil) {
                    return;
                }
                let idx = self.hash.len();
                self.hash.push(HashSlot {
                    key_value,
                    value: Some(value),
                });
                self.index.insert(norm, idx);
            }
        }
    }

    /// After appending to the array, pull contiguous integer successors out of
    /// the hash part into the array (the rehash that keeps the array dense).
    fn absorb_from_hash(&mut self) {
        loop {
            let next_key = LuaKey::Integer(self.array.len() as i64 + 1);
            let Some(&idx) = self.index.get(&next_key) else {
                break;
            };
            let Some(value) = self.hash[idx].value else {
                break;
            };
            self.array.push(value);
            self.hash[idx].value = None;
            self.index.remove(&next_key);
        }
    }

    /// Recomputes the array/hash split. The representation stays dense as it
    /// grows, so an explicit rehash only needs to absorb any successors that
    /// landed in the hash part; the histogram resize is deferred.
    #[cfg(any(test, feature = "conformance"))]
    pub fn rehash(&mut self) {
        self.absorb_from_hash();
    }

    /// A border `n` such that `t[n]` is non-`nil` and `t[n+1]` is `nil` — the
    /// value `#t` returns.
    #[must_use]
    pub fn length(&self) -> u64 {
        let n = self.array.len();
        if n > 0 && matches!(self.array[n - 1], RawValue::Nil) {
            // Trailing nil: binary search the array for a border.
            return self.array_border(n) as u64;
        }
        // Array is full to the end; if the hash continues at n+1, search there.
        if self.index.contains_key(&LuaKey::Integer(n as i64 + 1)) {
            return self.hash_border(n as u64);
        }
        n as u64
    }

    fn array_border(&self, mut hi: usize) -> usize {
        while hi > 0 {
            if !matches!(self.array[hi - 1], RawValue::Nil) {
                return hi;
            }
            hi -= 1;
        }
        0
    }

    fn hash_border(&self, start: u64) -> u64 {
        // Unbound search: double until a nil, then binary search the gap.
        let mut i = start;
        let mut j = start + 1;
        while self.int_present(j) {
            i = j;
            if j > u64::MAX / 2 {
                // Degenerate sparse table; linear fallback.
                let mut k = start + 1;
                while self.int_present(k) {
                    k += 1;
                }
                return k - 1;
            }
            j *= 2;
        }
        while j - i > 1 {
            let mid = i + (j - i) / 2;
            if self.int_present(mid) {
                i = mid;
            } else {
                j = mid;
            }
        }
        i
    }

    fn int_present(&self, i: u64) -> bool {
        if i >= 1 && i <= self.array.len() as u64 {
            return !matches!(self.array[(i - 1) as usize], RawValue::Nil);
        }
        if i > i64::MAX as u64 {
            return false;
        }
        match self.index.get(&LuaKey::Integer(i as i64)) {
            Some(&idx) => self.hash[idx].value.is_some(),
            None => false,
        }
    }

    /// The iterator step behind `next`/`pairs`. `None` key starts iteration;
    /// returns the next `(key, value)` pair, or `None` at the end. An unknown
    /// key returns `None` (the caller treats that as "invalid key to next").
    #[must_use]
    pub fn next(&self, key: RawValue) -> NextStep {
        // Resolve the starting cursor.
        let cursor = match key {
            RawValue::Nil => Cursor::ArrayStart,
            other => match normalize_key(other) {
                Some(LuaKey::Integer(i)) if self.array_slot(i).is_some() => {
                    Cursor::Array((i - 1) as usize)
                }
                Some(norm) => match self.index.get(&norm) {
                    Some(&idx) => Cursor::Hash(idx),
                    None => return NextStep::InvalidKey,
                },
                None => return NextStep::InvalidKey,
            },
        };
        self.advance(&cursor)
    }

    fn advance(&self, cursor: &Cursor) -> NextStep {
        let mut probe = match cursor {
            Cursor::ArrayStart => 0,
            Cursor::Array(i) => i + 1,
            Cursor::Hash(idx) => return self.advance_hash(idx + 1),
        };
        while probe < self.array.len() {
            if !matches!(self.array[probe], RawValue::Nil) {
                // Array keys are integer-valued numbers, not native integers.
                let key = RawValue::Number(array_key(probe));
                return NextStep::Pair(key, self.array[probe]);
            }
            probe += 1;
        }
        self.advance_hash(0)
    }

    fn advance_hash(&self, mut idx: usize) -> NextStep {
        while idx < self.hash.len() {
            if let Some(value) = self.hash[idx].value {
                return NextStep::Pair(self.hash[idx].key_value, value);
            }
            idx += 1;
        }
        NextStep::Done
    }

    /// The number of array slots (live or `nil`), for tests and sizing.
    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub fn array_len(&self) -> usize {
        self.array.len()
    }

    /// The number of slots a full scan of the table touches — every array slot plus
    /// every hash slot (live or not). A builtin that unconditionally walks the whole
    /// table as a single bytecode instruction (`table.maxn`, `table.clone`) charges this
    /// against the instruction budget so its `O(scan_len)` work is metered.
    #[must_use]
    pub fn scan_len(&self) -> usize {
        self.array.len() + self.hash.len()
    }

    /// A shallow copy for `table.clone`: the same entries and metatable, but a
    /// mutable result (the `readonly`/`safeenv` flags are not carried over) on a
    /// fresh orphan meter, charged when it is allocated into the heap.
    #[must_use]
    pub fn shallow_clone(&self) -> Self {
        Self {
            array: self.array.clone(),
            hash: self.hash.clone(),
            index: self.index.clone(),
            metatable: self.metatable,
            readonly: false,
            safeenv: false,
            meter: MemoryMeter::default(),
            charged: 0,
        }
    }

    #[cfg(any())]
    pub(crate) fn hash_for_key(&self, key: RawValue) -> Option<u64> {
        let norm = normalize_key(key)?;
        Some(self.index.hasher().hash_one(norm))
    }

    /// Empties the table for `table.clear`, keeping the reserved capacity (like
    /// upstream `luaH_clear`); the caller checks `readonly` first.
    pub fn clear(&mut self) {
        self.array.fill(RawValue::Nil);
        self.hash.clear();
        self.index.clear();
        self.recharge();
    }

    /// The largest numeric key with a non-`nil` value, or `0` (`table.maxn`).
    /// The array part contributes its highest live slot; the hash part its
    /// largest numeric key, matching upstream's two-part scan.
    #[must_use]
    pub fn maxn(&self) -> f64 {
        let mut max = 0.0_f64;
        for (slot, value) in self.array.iter().enumerate() {
            if !matches!(value, RawValue::Nil) {
                max = array_key(slot);
            }
        }
        for slot in &self.hash {
            if slot.value.is_none() {
                continue;
            }
            let key = match slot.key_value {
                RawValue::Number(n) => n,
                RawValue::Integer(i) => i as f64,
                _ => continue,
            };
            if key > max {
                max = key;
            }
        }
        max
    }

    pub(crate) fn snapshot_image(&self) -> LuaTableImage {
        LuaTableImage {
            array: self.array.clone(),
            hash: self
                .hash
                .iter()
                .map(|slot| HashSlotImage {
                    key_value: slot.key_value,
                    value: slot.value,
                })
                .collect(),
            metatable: self.metatable,
            readonly: self.readonly,
            safeenv: self.safeenv,
        }
    }

    pub(crate) fn from_snapshot_image(
        image: LuaTableImage,
        hash_builder: VmBuildHasher,
        meter: MemoryMeter,
    ) -> Self {
        let mut table = Self {
            array: image.array,
            hash: image
                .hash
                .into_iter()
                .map(|slot| HashSlot {
                    key_value: slot.key_value,
                    value: slot.value,
                })
                .collect(),
            index: std::collections::HashMap::with_hasher(hash_builder),
            metatable: image.metatable,
            readonly: image.readonly,
            safeenv: image.safeenv,
            meter,
            charged: 0,
        };
        for (idx, slot) in table.hash.iter().enumerate() {
            if slot.value.is_some()
                && let Some(norm) = normalize_key(slot.key_value)
            {
                table.index.insert(norm, idx);
            }
        }
        table.recharge();
        table
    }
}

impl Default for LuaTable {
    fn default() -> Self {
        Self::new()
    }
}

enum Cursor {
    ArrayStart,
    Array(usize),
    Hash(usize),
}

/// The result of a [`LuaTable::next`] step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NextStep {
    /// The next live key/value pair.
    Pair(RawValue, RawValue),
    /// Iteration is complete.
    Done,
    /// The supplied key is not in the table (invalid argument to `next`).
    InvalidKey,
}

#[cfg(any())]
mod tests {
    use super::*;

    /// An integer-valued number — what a Lua source literal like `1` actually is,
    /// and the key the array part holds.
    #[allow(clippy::cast_precision_loss)]
    fn num(i: i64) -> RawValue {
        RawValue::Number(i as f64)
    }

    /// A native 64-bit integer (`RawValue::Integer`) — distinct from `num`.
    fn int(i: i64) -> RawValue {
        RawValue::Integer(i)
    }

    #[test]
    fn get_set_array_and_hash() {
        let mut t = LuaTable::new();
        assert_eq!(t.get(num(1)), RawValue::Nil);
        t.set(num(1), RawValue::Boolean(true));
        t.set(num(2), num(20));
        t.set(RawValue::Boolean(true), num(99));
        assert_eq!(t.get(num(1)), RawValue::Boolean(true));
        assert_eq!(t.get(num(2)), num(20));
        assert_eq!(t.get(RawValue::Boolean(true)), num(99));
        assert_eq!(t.array_len(), 2);
    }

    #[test]
    fn native_int_and_number_keys_are_distinct() {
        // The revision keys `t[1]` (number) and `t[1]`-as-native-integer apart.
        let mut t = LuaTable::new();
        t.set(num(1), RawValue::Boolean(true));
        t.set(int(1), RawValue::Boolean(false));
        assert_eq!(t.get(num(1)), RawValue::Boolean(true));
        assert_eq!(t.get(int(1)), RawValue::Boolean(false));
        // The number key lives in the array; the native integer does not.
        assert_eq!(t.array_len(), 1);
    }

    #[test]
    fn nil_and_nan_keys_rejected() {
        let mut t = LuaTable::new();
        assert_eq!(key_rejection(RawValue::Nil), Some(KeyRejection::Nil));
        assert_eq!(
            key_rejection(RawValue::Number(f64::NAN)),
            Some(KeyRejection::NaN)
        );
        assert_eq!(
            key_rejection(RawValue::Vector([1.0, f32::NAN, 3.0])),
            Some(KeyRejection::NaN)
        );
        assert!(!t.set(RawValue::Nil, num(1)));
        assert!(!t.set(RawValue::Number(f64::NAN), num(1)));
        assert_eq!(t.get(RawValue::Nil), RawValue::Nil);
    }

    #[test]
    fn hash_part_absorbs_into_array() {
        let mut t = LuaTable::new();
        t.set(num(2), num(20));
        t.set(num(3), num(30));
        assert_eq!(t.array_len(), 0); // 2 and 3 are not contiguous from 1
        t.set(num(1), num(10));
        assert_eq!(t.array_len(), 3); // appending 1 absorbs 2 and 3
        assert_eq!(t.length(), 3);
    }

    #[test]
    fn length_border() {
        let mut t = LuaTable::new();
        for i in 1..=5 {
            t.set(num(i), num(i * 10));
        }
        assert_eq!(t.length(), 5);
        t.set(num(5), RawValue::Nil);
        assert_eq!(t.length(), 4);
    }

    #[test]
    fn clear_preserves_array_shape() {
        let mut cleared = LuaTable::with_array(vec![RawValue::Nil; 16]);
        for i in 1..=16 {
            cleared.set(num(i), num(i));
        }
        cleared.clear();
        assert_eq!(cleared.array_len(), 16);
        assert_eq!(cleared.length(), 0);
        cleared.set(num(2), RawValue::Boolean(true));

        let mut created = LuaTable::with_array(vec![RawValue::Nil; 16]);
        created.set(num(2), RawValue::Boolean(true));
        assert_eq!(cleared.length(), created.length());
    }

    #[test]
    fn next_visits_every_live_entry_once() {
        let mut t = LuaTable::new();
        t.set(num(1), num(10));
        t.set(num(2), num(20));
        t.set(RawValue::Boolean(false), num(99));
        let mut seen = Vec::new();
        let mut key = RawValue::Nil;
        loop {
            match t.next(key) {
                NextStep::Pair(k, v) => {
                    seen.push((k, v));
                    key = k;
                }
                NextStep::Done => break,
                NextStep::InvalidKey => panic!("unexpected invalid key"),
            }
        }
        assert_eq!(seen.len(), 3);
        // Array keys come back as integer-valued numbers.
        assert!(seen.contains(&(num(1), num(10))));
        assert!(seen.contains(&(num(2), num(20))));
        assert!(seen.contains(&(RawValue::Boolean(false), num(99))));
    }

    #[test]
    fn next_allows_clearing_current_key() {
        let mut t = LuaTable::new();
        t.set(RawValue::Boolean(true), num(1));
        t.set(RawValue::Boolean(false), num(2));
        let mut count = 0;
        let mut key = RawValue::Nil;
        while let NextStep::Pair(k, _) = t.next(key) {
            count += 1;
            key = k;
            t.set(k, RawValue::Nil); // clearing the current key mid-traversal is allowed
            if count > 10 {
                panic!("did not terminate");
            }
        }
        assert_eq!(count, 2);
    }
}
