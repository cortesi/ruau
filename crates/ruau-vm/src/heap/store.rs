use std::collections::TryReserveError;

use ruau_vm_api::{RawGc, RawValue};

use super::{Arena, MemoryMeter};
use crate::{
    func::{Closure, UpVal},
    object::{LuaBuffer, LuaUserdata, Proto},
    state::Thread,
    string::InternedString,
    table::LuaTable,
};

/// The object arenas, a `Heap` field disjoint from the register stacks so the
/// interpreter can borrow registers and mutate objects at once.
pub struct ObjectStore {
    /// Interned strings.
    pub strings: Arena<InternedString>,
    /// Tables.
    pub tables: Arena<LuaTable>,
    /// Closures.
    pub closures: Arena<Closure>,
    /// Host userdata.
    pub userdata: Arena<LuaUserdata>,
    /// Threads / coroutines.
    pub threads: Arena<Thread>,
    /// Byte buffers.
    pub buffers: Arena<LuaBuffer>,
    /// Loaded prototypes.
    pub protos: Arena<Proto>,
    /// Upvalue cells.
    pub upvals: Arena<UpVal>,
}

impl ObjectStore {
    pub(super) fn with_meter(meter: &MemoryMeter) -> Self {
        Self {
            strings: Arena::with_meter(meter.clone()),
            tables: Arena::with_meter(meter.clone()),
            closures: Arena::with_meter(meter.clone()),
            userdata: Arena::with_meter(meter.clone()),
            threads: Arena::with_meter(meter.clone()),
            buffers: Arena::with_meter(meter.clone()),
            protos: Arena::with_meter(meter.clone()),
            upvals: Arena::with_meter(meter.clone()),
        }
    }

    /// GC: reserve every arena's free list before a sweep so reclamation cannot
    /// allocate (and abort the process) mid-sweep. Fails as a unit — if any arena's
    /// reservation fails the collector aborts the cycle before sweeping anything.
    ///
    /// # Errors
    /// Returns `TryReserveError` if any free-list reservation would exceed memory.
    pub(crate) fn gc_reserve_free_lists(&mut self) -> Result<(), TryReserveError> {
        self.strings.gc_reserve_free()?;
        self.tables.gc_reserve_free()?;
        self.closures.gc_reserve_free()?;
        self.userdata.gc_reserve_free()?;
        self.threads.gc_reserve_free()?;
        self.buffers.gc_reserve_free()?;
        self.protos.gc_reserve_free()?;
        self.upvals.gc_reserve_free()?;
        Ok(())
    }

    /// GC: like [`gc_reserve_free_lists`](Self::gc_reserve_free_lists) but for a minor
    /// sweep (young-white slots only).
    ///
    /// # Errors
    /// Returns `TryReserveError` if any free-list reservation would exceed memory.
    pub(crate) fn gc_reserve_free_lists_minor(&mut self) -> Result<(), TryReserveError> {
        self.strings.gc_reserve_free_minor()?;
        self.tables.gc_reserve_free_minor()?;
        self.closures.gc_reserve_free_minor()?;
        self.userdata.gc_reserve_free_minor()?;
        self.threads.gc_reserve_free_minor()?;
        self.buffers.gc_reserve_free_minor()?;
        self.protos.gc_reserve_free_minor()?;
        self.upvals.gc_reserve_free_minor()?;
        Ok(())
    }

    /// GC: release every arena's reclaimed trailing capacity after a sweep (see
    /// [`Arena::gc_compact`]). Infallible — it only returns memory.
    pub(crate) fn gc_compact_all(&mut self) {
        self.strings.gc_compact();
        self.tables.gc_compact();
        self.closures.gc_compact();
        self.userdata.gc_compact();
        self.threads.gc_compact();
        self.buffers.gc_compact();
        self.protos.gc_compact();
        self.upvals.gc_compact();
    }

    /// Observable live heap footprint for `gcinfo()`/`collectgarbage("count")`.
    /// This is deliberately live-set based; the service cap continues to use the
    /// conservative high-water [`MemoryMeter`] via [`Heap::total_bytes`].
    pub(super) fn gc_live_bytes(&self) -> usize {
        self.strings
            .gc_live_footprint_with(InternedString::gc_footprint)
            + self.tables.gc_live_footprint_with(LuaTable::gc_footprint)
            + self.closures.gc_live_footprint_with(|c| {
                c.upvals.capacity() * std::mem::size_of::<RawGc<UpVal>>()
            })
            + self
                .userdata
                .gc_live_footprint_with(LuaUserdata::gc_footprint)
            + self
                .threads
                .gc_live_footprint_with(Thread::gc_live_footprint)
            + self.buffers.gc_live_footprint_with(LuaBuffer::gc_footprint)
            + self.protos.gc_live_footprint_with(Proto::footprint)
            + self.upvals.gc_live_footprint_with(|_| 0)
    }
}

/// The register stack, the active-stack lease. A field disjoint from
/// the object arenas so the interpreter can address registers while it mutates
/// objects. Registers are a flat slot array addressed by absolute index.
///
/// Real growth happens only at frame entry through [`ensure`](Self::ensure),
/// which charges via `try_reserve` so a hostile stack request fails gracefully
/// rather than aborting. The interpreter sizes each frame's window before
/// running it, so the hot [`get`](Self::get)/[`set`](Self::set) paths only touch
/// already-resident slots.
pub struct StackStore {
    slots: Vec<RawValue>,
    meter: MemoryMeter,
    charged: usize,
}

impl Default for StackStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StackStore {
    /// An empty register stack charging an orphan meter until its thread joins a
    /// heap (see [`attach_meter`](Self::attach_meter)).
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            meter: MemoryMeter::default(),
            charged: 0,
        }
    }

    /// Points this stack at the heap's shared meter and charges its current
    /// footprint — called when the owning thread is allocated into the heap. A
    /// clean hand-off: it first releases its charge from the previous meter, so a
    /// second attach (or a re-home) never double-counts or strands a charge.
    pub fn attach_meter(&mut self, meter: MemoryMeter) {
        self.meter.adjust(self.charged, 0);
        self.meter = meter;
        self.charged = 0;
        self.recharge();
    }

    /// Reconciles the meter with the stack's current capacity after a grow.
    fn recharge(&mut self) {
        let now = self.slots.capacity() * std::mem::size_of::<RawValue>();
        self.meter.adjust(self.charged, now);
        self.charged = now;
    }

    pub(super) fn snapshot_slots(&self) -> Vec<RawValue> {
        self.slots.clone()
    }

    pub(super) fn from_snapshot_slots(slots: Vec<RawValue>, meter: MemoryMeter) -> Self {
        let mut store = Self {
            slots,
            meter,
            charged: 0,
        };
        store.recharge();
        store
    }

    /// The register at an absolute index (`nil` past the end).
    #[must_use]
    #[inline]
    pub fn get(&self, index: u32) -> RawValue {
        self.slots
            .get(index as usize)
            .copied()
            .unwrap_or(RawValue::Nil)
    }

    /// GC: resident registers up to the live stack top. Slots above that point are
    /// stale capacity from earlier, larger frames and must not retain collectable values.
    pub(crate) fn gc_slots_up_to(&self, top: u32) -> &[RawValue] {
        let top = (top as usize).min(self.slots.len());
        &self.slots[..top]
    }

    /// GC: the metered register footprint to release when the owning thread is
    /// swept. Frame-owned vararg side storage releases itself when frames are
    /// popped or the thread is dropped.
    pub(crate) fn gc_footprint(&self) -> usize {
        self.charged
    }

    /// Writes the register at an absolute index within the resident window. The
    /// interpreter sizes a frame with [`ensure`](Self::ensure) before running
    /// it, so a well-formed write lands in place; an out-of-window index (a
    /// frame-sizing bug, never hostile bytecode — register operands are bounded
    /// at load) grows with `nil` rather than panicking.
    #[inline]
    pub fn set(&mut self, index: u32, value: RawValue) {
        let index = index as usize;
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = value;
        } else {
            self.set_grow(index, value);
        }
    }

    /// Grow path for [`Self::set`], out of line so the resident-slot store
    /// stays small enough to inline into the dispatch arms.
    #[cold]
    fn set_grow(&mut self, index: usize, value: RawValue) {
        self.slots.resize(index + 1, RawValue::Nil);
        self.recharge();
        self.slots[index] = value;
    }

    /// Ensures at least `len` slots are resident, filling new ones with `nil`.
    /// Charges the reservation first.
    ///
    /// # Errors
    /// Returns `TryReserveError` if the reservation would exceed available
    /// memory (the graceful first line before the process backstop).
    pub fn ensure(&mut self, len: u32) -> Result<(), TryReserveError> {
        let len = len as usize;
        if len > self.slots.len() {
            self.slots.try_reserve(len - self.slots.len())?;
            self.slots.resize(len, RawValue::Nil);
            self.recharge();
        }
        Ok(())
    }
}
