use std::{
    collections::TryReserveError,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::{ArenaEntryImage, ArenaImage};
use crate::snapshot::SnapshotError;

/// Tri-color GC mark state, stored inline in each arena entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum Color {
    /// Not yet reached this cycle.
    White,
    /// Reached but children not yet scanned.
    Gray,
    /// Reached and fully scanned.
    Black,
}

/// Generational age, inline in each arena entry. Orthogonal to [`Color`] (the
/// per-cycle mark) and to a slot's handle generation (staleness rejection).
///
/// A new object is [`Young`](Self::Young). A minor collection promotes every
/// young survivor to [`Old`](Self::Old), so the only old→young edges left are
/// those a later store creates; the write barrier flips such an old holder to
/// [`OldRemembered`](Self::OldRemembered) and records it, so the next minor
/// traces it as a root. A minor never traces through or sweeps a plain `Old`
/// object — that is the generational win.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum Age {
    /// Allocated since the last collection; a minor traces and sweeps it.
    Young,
    /// Survived a collection; a minor skips it (presumed live, no young children).
    Old,
    /// `Old`, but the write barrier recorded a stored young reference, so a minor
    /// traces it as a root. Reverts to `Old` after the minor (which promotes the
    /// stored young objects, so the edge becomes old→old).
    OldRemembered,
}

/// An arena slot: GC color inline with the value, so a color/value check is one
/// contiguous read. The generation stamp lives in a parallel `gens`
/// vector rather than here, so the value array can be truncated to release a swept
/// slot's capacity while the generation sequence for that index survives (a regrown
/// slot must not let a stale handle alias the new object).
pub struct ArenaEntry<T> {
    /// Mark color for the current GC cycle.
    pub color: Color,
    /// Generational age, for the minor/major split.
    pub age: Age,
    /// The owned object, or `None` for a free (swept) slot.
    pub value: Option<T>,
}

/// A process-wide `Send` byte counter for one VM's heap: every
/// growable container charges its capacity here, and the dispatch safepoint
/// compares the total against [`Limits::max_memory_bytes`](crate::Limits) so one
/// VM cannot outgrow its share before the process backstop. It is atomic only to
/// stay `Send` for the multi-thread runtime — a single VM is touched by one task
/// at a time and never actually races, so `Relaxed` ordering suffices.
///
/// A container charges on growth; release happens at collection — the sweep drops each
/// reclaimed object's payload footprint, and post-sweep compaction
/// ([`Arena::gc_compact`]) returns a truncated arena tail's slot-vector capacity. Between
/// collections a heap's footprint only rises, the conservative direction for a cap. A
/// `Default` meter is an orphan a standalone container (a unit test, a not-yet-allocated
/// object) charges harmlessly.
#[derive(Clone, Default)]
pub struct MemoryMeter {
    used: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl MemoryMeter {
    /// Adjusts the total by a container's footprint change (old → new). The
    /// decrease path saturates at zero: an accounting drift must never wrap the
    /// counter to `usize::MAX` and wedge the VM in a permanent "over cap" state.
    pub fn adjust(&self, old: usize, new: usize) {
        if new >= old {
            let used = self.used.fetch_add(new - old, Ordering::Relaxed) + (new - old);
            self.peak.fetch_max(used, Ordering::Relaxed);
        } else {
            let delta = old - new;
            // The closure always returns `Some`, so `fetch_update` never reports
            // `Err`; discard the prior value rather than binding it to `_`.
            self.used
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                    Some(used.saturating_sub(delta))
                })
                .ok();
        }
    }

    /// Charges `bytes` to the total — for a payload sized once at allocation (an
    /// interned string's bytes), not a reservable container capacity.
    pub fn charge(&self, bytes: usize) {
        let used = self.used.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(used, Ordering::Relaxed);
    }

    /// The current charged byte total.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// The highest charged byte total observed by this meter.
    #[must_use]
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
}

/// A `Vec` that charges before it grows, using `try_reserve` so an allocation
/// that would exceed the cap fails gracefully rather than aborting, and reports
/// its capacity to the heap's [`MemoryMeter`] so the per-VM cap can be enforced
/// at the dispatch safepoint.
pub struct AccountedVec<T> {
    pub(super) inner: Vec<T>,
    meter: MemoryMeter,
    charged: usize,
}

impl<T> AccountedVec<T> {
    /// Whether the vector is empty.
    #[cfg(any())]
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// An empty accounted vector charging an orphan meter (standalone use/tests).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Vec::new(),
            meter: MemoryMeter::default(),
            charged: 0,
        }
    }

    /// An empty accounted vector charging the heap's shared meter.
    #[must_use]
    pub fn with_meter(meter: MemoryMeter) -> Self {
        Self {
            inner: Vec::new(),
            meter,
            charged: 0,
        }
    }

    pub(super) fn from_vec(inner: Vec<T>, meter: MemoryMeter) -> Self {
        let mut this = Self {
            inner,
            meter,
            charged: 0,
        };
        this.recharge();
        this
    }

    /// Reconciles the meter with the vector's current capacity after a possible
    /// growth.
    fn recharge(&mut self) {
        let now = self.inner.capacity() * std::mem::size_of::<T>();
        self.meter.adjust(self.charged, now);
        self.charged = now;
    }

    /// Pushes a value, reserving first; returns the slot index.
    ///
    /// # Errors
    /// Returns `TryReserveError` if the reservation would exceed available
    /// memory (the graceful first line before the process backstop).
    pub fn try_push(&mut self, value: T) -> Result<usize, TryReserveError> {
        self.inner.try_reserve(1)?;
        let index = self.inner.len();
        self.inner.push(value);
        self.recharge();
        Ok(index)
    }

    /// A shared reference to the slot at `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&T> {
        self.inner.get(index)
    }

    /// A mutable reference to the slot at `index`.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.inner.get_mut(index)
    }

    /// The number of elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Truncates to `len` elements and releases the freed capacity back to the meter.
    /// A no-op when `len` is not below the current length. The GC sweep uses this to
    /// return a reclaimed arena tail's capacity.
    pub fn truncate(&mut self, len: usize) {
        if len < self.inner.len() {
            self.inner.truncate(len);
            self.inner.shrink_to_fit();
            self.recharge();
        }
    }
}

impl<T> Default for AccountedVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A typed arena of objects of one kind, backed by accounted storage with a free
/// list for swept slots.
pub struct Arena<T> {
    entries: AccountedVec<ArenaEntry<T>>,
    /// Generation stamp per slot index, parallel to `entries` but never truncated:
    /// `gens.len()` is the high-water of indices ever allocated. Keeping it separate
    /// lets the sweep truncate `entries` to release a reclaimed tail's capacity while
    /// the generation sequence persists, so a regrown index resumes its sequence and a
    /// stale `(index, old_gen)` handle can never match the new object at that slot.
    pub(super) gens: AccountedVec<u32>,
    /// Indices of swept slots available for reuse. The sweep sorts this descending, so
    /// [`alloc`](Self::alloc)'s `pop` reuses the lowest free index first, keeping the
    /// live set clustered low so the trailing free tail stays truncatable.
    free: Vec<u32>,
    /// Indices of `Young` slots — every object allocated since the last collection. A minor
    /// sweep drains this instead of scanning all slots, so its cost is bounded by the young
    /// set rather than the arena high-water (the generational win). `alloc` pushes;
    /// `gc_sweep_minor_with` drains (freeing young white, promoting young survivors to
    /// `Old`); the major sweep clears it (a major promotes every survivor old). The list is
    /// a superset of the live young slots — a slot freed/promoted by a minor is dropped on
    /// drain, and a major clears it — so a stale entry is skipped, never double-freed.
    young: Vec<u32>,
}

impl<T> Arena<T> {
    /// Whether the arena has no slots.
    #[cfg(any())]
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// An empty arena charging an orphan meter (standalone use/tests).
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: AccountedVec::new(),
            gens: AccountedVec::new(),
            free: Vec::new(),
            young: Vec::new(),
        }
    }

    /// An empty arena charging the heap's shared meter.
    #[must_use]
    pub fn with_meter(meter: MemoryMeter) -> Self {
        Self {
            entries: AccountedVec::with_meter(meter.clone()),
            gens: AccountedVec::with_meter(meter),
            free: Vec::new(),
            young: Vec::new(),
        }
    }

    pub(super) fn from_snapshot_entries(
        entries: Vec<ArenaEntry<T>>,
        gens: Vec<u32>,
        free: Vec<u32>,
        young: Vec<u32>,
        meter: MemoryMeter,
    ) -> Self {
        Self {
            entries: AccountedVec::from_vec(entries, meter.clone()),
            gens: AccountedVec::from_vec(gens, meter),
            free,
            young,
        }
    }

    pub(super) fn snapshot_image_with<U>(
        &self,
        mut snapshot: impl FnMut(&T) -> Result<U, SnapshotError>,
    ) -> Result<ArenaImage<U>, SnapshotError> {
        let mut entries = Vec::with_capacity(self.entries.len());
        for index in 0..self.entries.len() {
            let entry = self
                .entries
                .get(index)
                .expect("index is within entries length");
            entries.push(ArenaEntryImage {
                color: entry.color,
                age: entry.age,
                value: entry.value.as_ref().map(&mut snapshot).transpose()?,
            });
        }
        Ok(ArenaImage {
            entries,
            gens: self.gens.inner.clone(),
            free: self.free.clone(),
            young: self.young.clone(),
        })
    }

    /// Allocates `value`, returning its `(index, generation)`. Reuses a swept
    /// slot when one is free (its generation was already bumped on free).
    ///
    /// # Errors
    /// Returns `TryReserveError` if growing the arena would exceed memory.
    pub fn alloc(&mut self, value: T) -> Result<(u32, u32), TryReserveError> {
        // Reserve the young-list slot first so the post-allocation push is infallible: every
        // new (`Young`) object must land in the young list, or a minor sweep — which only
        // visits that list — would never reclaim it. Reserving up front means a young-list
        // OOM fails the allocation cleanly (no orphaned entry) rather than leaving an
        // unlisted young slot.
        self.young.try_reserve(1)?;
        if let Some(index) = self.free.pop() {
            let entry = self
                .entries
                .get_mut(index as usize)
                .expect("free index resident");
            entry.value = Some(value);
            entry.color = Color::White;
            entry.age = Age::Young;
            let generation = *self.gens.get(index as usize).expect("gen resident");
            self.young.push(index);
            Ok((index, generation))
        } else {
            let index = u32::try_from(self.entries.len()).expect("arena index fits u32");
            // A brand-new index needs a generation slot; a regrown (previously truncated)
            // index already has one in `gens` and resumes its sequence. Reserve `gens` first
            // so that if the `entries` push fails the arena stays consistent (`gens` merely
            // runs one ahead, which the next allocation reuses).
            if index as usize >= self.gens.len() {
                self.gens.try_push(0)?;
            }
            self.entries.try_push(ArenaEntry {
                color: Color::White,
                age: Age::Young,
                value: Some(value),
            })?;
            let generation = *self.gens.get(index as usize).expect("gen resident");
            self.young.push(index);
            Ok((index, generation))
        }
    }

    /// A shared reference to the live object at `(index, generation)`, or `None`
    /// for a stale or freed handle.
    #[must_use]
    pub fn get(&self, index: u32, generation: u32) -> Option<&T> {
        if self.gens.get(index as usize).copied() != Some(generation) {
            return None;
        }
        self.entries.get(index as usize)?.value.as_ref()
    }

    /// A mutable reference to the live object at `(index, generation)`.
    pub fn get_mut(&mut self, index: u32, generation: u32) -> Option<&mut T> {
        if self.gens.get(index as usize).copied() != Some(generation) {
            return None;
        }
        self.entries.get_mut(index as usize)?.value.as_mut()
    }

    /// Frees the slot at `index`, bumping its generation so existing handles go
    /// stale. Used by the sweep phase.
    pub fn free(&mut self, index: u32) {
        if let Some(entry) = self.entries.get_mut(index as usize) {
            entry.value = None;
            // `gens.len() >= entries.len()` always holds (alloc reserves `gens` first), so a
            // live slot always has a generation. The bump is what makes outstanding handles
            // stale; skipping it would let a reused slot alias them, so assert the invariant
            // rather than silently drop the bump.
            debug_assert!(
                (index as usize) < self.gens.len(),
                "every arena slot has a generation"
            );
            if let Some(generation) = self.gens.get_mut(index as usize) {
                *generation = generation.wrapping_add(1);
            }
            self.free.push(index);
        }
    }

    /// The number of slots, live or free.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Observable GC footprint for live slots: an occupied arena header plus the
    /// object's own metered payload. Unlike [`MemoryMeter::used`], this intentionally
    /// ignores free slots retained for reuse so `gcinfo()` reflects reclamation.
    pub(crate) fn gc_live_footprint_with(&self, mut payload: impl FnMut(&T) -> usize) -> usize {
        let mut bytes = 0usize;
        for index in 0..self.entries.len() {
            if let Some(value) = self
                .entries
                .get(index)
                .and_then(|entry| entry.value.as_ref())
            {
                bytes = bytes
                    .saturating_add(std::mem::size_of::<ArenaEntry<T>>())
                    .saturating_add(payload(value));
            }
        }
        bytes
    }

    /// GC: the mark color of slot `index` (`White` for an out-of-range slot).
    pub(crate) fn gc_color(&self, index: u32) -> Color {
        self.entries
            .get(index as usize)
            .map_or(Color::White, |entry| entry.color)
    }

    /// GC: set the mark color of slot `index`.
    pub(crate) fn gc_set_color(&mut self, index: u32, color: Color) {
        if let Some(entry) = self.entries.get_mut(index as usize) {
            entry.color = color;
        }
    }

    /// GC: the generational age of slot `index` (`Old` for an out-of-range slot, so a
    /// stale index is never mistaken for collectable young).
    pub(crate) fn gc_age(&self, index: u32) -> Age {
        self.entries
            .get(index as usize)
            .map_or(Age::Old, |entry| entry.age)
    }

    /// GC: set the generational age of slot `index`.
    pub(crate) fn gc_set_age(&mut self, index: u32, age: Age) {
        if let Some(entry) = self.entries.get_mut(index as usize) {
            entry.age = age;
        }
    }

    /// GC: the live value at `index` regardless of handle generation — for tracing
    /// an already-marked object's children.
    pub(crate) fn gc_value(&self, index: u32) -> Option<&T> {
        self.entries
            .get(index as usize)
            .and_then(|e| e.value.as_ref())
    }

    /// GC: a mutable reference to the live value at `index` — for the weak-table atomic
    /// clear pass, which removes dead entries in place after marking.
    ///
    /// **Generational soundness:** this bypasses the [`table_mut`](Heap::table_mut) /
    /// [`upval_mut`](Heap::upval_mut) write barrier, so a caller must not use it to store a
    /// reference that could create an unrecorded old→young edge. The one caller
    /// (`clear_weak_tables`) only *nils* already-dead entries inside the collector after
    /// marking, which creates no surviving edge. A new caller that stores live references
    /// here would need its own `gc::remember`.
    pub(crate) fn gc_value_mut(&mut self, index: u32) -> Option<&mut T> {
        self.entries
            .get_mut(index as usize)
            .and_then(|e| e.value.as_mut())
    }

    /// GC major sweep: free every live slot still `White` (unreached), calling `on_free`
    /// with each object just before it is freed — the collector releases that object's
    /// metered byte footprint there — and resetting each reached (`Black`/`Gray`) slot to
    /// `White` *and promoting it to `Old`* for the next cycle. Returns the count freed.
    /// A major settles every survivor into the old generation, so subsequent minors skip
    /// them.
    pub(crate) fn gc_sweep_with(&mut self, mut on_free: impl FnMut(&T)) -> usize {
        let mut freed = 0;
        for index in 0..self.entries.len() as u32 {
            let (is_white, is_live) = match self.entries.get(index as usize) {
                Some(entry) => (entry.color == Color::White, entry.value.is_some()),
                None => continue,
            };
            if !is_live {
                continue; // already a free slot
            }
            if is_white {
                if let Some(value) = self
                    .entries
                    .get(index as usize)
                    .and_then(|e| e.value.as_ref())
                {
                    on_free(value);
                }
                self.free(index); // the `get` borrow above has ended
                freed += 1;
            } else if let Some(entry) = self.entries.get_mut(index as usize) {
                entry.color = Color::White;
                entry.age = Age::Old;
            }
        }
        // A major promoted every survivor to `Old`, so no `Young` slots remain; drop the
        // young list (its indices are now old or freed) so the next minor starts fresh.
        self.young.clear();
        freed
    }

    /// GC minor sweep: free every unreached `Young` slot and promote every reached one to
    /// `Old`, draining the young list (which holds exactly the slots allocated since the
    /// last collection) so the cost is bounded by the young set, not the arena high-water.
    /// Returns the count freed (young only — old garbage is a major's job).
    ///
    /// This touches *only* young slots. The minor mark also blackened some old slots (the
    /// roots, remembered holders, and threads); resetting their color and reverting
    /// `OldRemembered`→`Old` is the caller's job (`collect_minor_inner`, via the `marked`
    /// list), because a minor must not scan the whole arena. A young-list entry that is no
    /// longer a live `Young` slot (defensively) is skipped, never double-freed.
    pub(crate) fn gc_sweep_minor_with(&mut self, mut on_free: impl FnMut(&T)) -> usize {
        let mut freed = 0;
        let young = std::mem::take(&mut self.young);
        for index in young {
            let (color, age, is_live) = match self.entries.get(index as usize) {
                Some(entry) => (entry.color, entry.age, entry.value.is_some()),
                None => continue,
            };
            if !is_live || age != Age::Young {
                continue; // freed or already promoted — not this minor's to handle
            }
            if color == Color::White {
                if let Some(value) = self
                    .entries
                    .get(index as usize)
                    .and_then(|e| e.value.as_ref())
                {
                    on_free(value);
                }
                self.free(index);
                freed += 1;
            } else if let Some(entry) = self.entries.get_mut(index as usize) {
                entry.color = Color::White; // promote and clear the mark
                entry.age = Age::Old;
            }
        }
        freed
    }

    /// Minor sweep with no per-object hook (closures, upvalues, userdata).
    pub(crate) fn gc_sweep_minor(&mut self) -> usize {
        self.gc_sweep_minor_with(|_| {})
    }

    /// GC sweep with no per-object hook — for arenas whose objects carry no metered
    /// byte footprint (closures, upvalues, userdata).
    pub(crate) fn gc_sweep(&mut self) -> usize {
        self.gc_sweep_with(|_| {})
    }

    /// Reset every live slot to `White` without freeing anything — the collector's
    /// abort-recovery path, scrubbing a half-finished cycle's marks so the next
    /// collection starts from a clean color state.
    pub(crate) fn gc_reset_colors(&mut self) {
        for index in 0..self.entries.len() {
            if let Some(entry) = self.entries.get_mut(index)
                && entry.value.is_some()
            {
                entry.color = Color::White;
            }
        }
    }

    /// Reserve free-list capacity for the live `White` slots the next [`Self::gc_sweep`]
    /// will free, so that sweep's `free.push` cannot allocate (and abort the process)
    /// after it has begun releasing object state. Call once per arena, after marking,
    /// before sweeping.
    ///
    /// # Errors
    /// Returns `TryReserveError` if the reservation would exceed available memory; the
    /// collector aborts the cycle (resetting colors, sweeping nothing) on that.
    pub(crate) fn gc_reserve_free(&mut self) -> Result<(), TryReserveError> {
        let dead = (0..self.entries.len())
            .filter(|&index| {
                self.entries
                    .get(index)
                    .is_some_and(|entry| entry.value.is_some() && entry.color == Color::White)
            })
            .count();
        self.free.try_reserve(dead)
    }

    /// Like [`gc_reserve_free`](Self::gc_reserve_free) but for a minor sweep, which frees at
    /// most one slot per young-list entry. Reserving the young-list length is an `O(1)` upper
    /// bound (some young survive and are promoted rather than freed), so the minor reserve —
    /// like the minor sweep — never scans the whole arena.
    pub(crate) fn gc_reserve_free_minor(&mut self) -> Result<(), TryReserveError> {
        self.free.try_reserve(self.young.len())
    }

    /// GC: after a sweep, release the arena's trailing free capacity. Truncates the value
    /// vector to the highest live index and shrinks it, returning that memory to the meter,
    /// then sorts the surviving free list descending so subsequent allocations reuse the
    /// lowest index first — keeping the live set low and the tail truncatable next cycle.
    ///
    /// The `gens` vector is never truncated, so a reclaimed index that is later regrown
    /// resumes its generation sequence and a stale handle to its previous occupant stays
    /// rejected. The cost is a residual floor of one `u32` per index ever allocated (the
    /// high-water), which compaction never reclaims; it is bounded by the arena's peak slot
    /// count and is dwarfed by the per-slot `ArenaEntry<T>` capacity that truncation does
    /// release, so the heap still shrinks materially after a spike — just not to zero.
    ///
    /// Infallible: it only releases memory (truncation does not allocate, and the sort is in
    /// place).
    pub(crate) fn gc_compact(&mut self) {
        let live_len = (0..self.entries.len())
            .rev()
            .find(|&index| {
                self.entries
                    .get(index)
                    .is_some_and(|entry| entry.value.is_some())
            })
            .map_or(0, |index| index + 1);
        if live_len < self.entries.len() {
            self.entries.truncate(live_len);
            self.free.retain(|&index| (index as usize) < live_len);
        }
        // Lowest-index-first reuse keeps the live set clustered low so a *future* cycle can
        // truncate the tail, so the sort runs whenever there are free slots — not only on a
        // cycle that itself truncated. Skip the trivial empty case.
        if !self.free.is_empty() {
            self.free.sort_unstable_by(|a, b| b.cmp(a));
        }
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}
