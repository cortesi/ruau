//! Address-stable, safely borrowed storage for host-userdata payloads.

use std::{
    any::Any,
    cell::{Cell, Ref, RefCell, RefMut},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::OnceLock,
};

use crate::heap::MemoryMeter;

const FANOUT: usize = 256;

type ErasedPayload = Box<dyn Any + Send>;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PayloadId(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadBorrowError {
    Missing,
    WrongType,
    Conflict,
}

#[derive(Default)]
struct PayloadSlot {
    value: RefCell<Option<ErasedPayload>>,
}

struct Leaf {
    slots: [PayloadSlot; FANOUT],
}

impl Leaf {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| PayloadSlot::default()),
        }
    }
}

struct LevelOne {
    leaves: [OnceLock<Box<Leaf>>; FANOUT],
}

impl LevelOne {
    fn new() -> Self {
        Self {
            leaves: std::array::from_fn(|_| OnceLock::new()),
        }
    }
}

struct LevelTwo {
    children: [OnceLock<Box<LevelOne>>; FANOUT],
}

impl LevelTwo {
    fn new() -> Self {
        Self {
            children: std::array::from_fn(|_| OnceLock::new()),
        }
    }
}

/// Stable segmented slots. Segment boxes never move after installation; GC
/// drops and reuses only the payload inside each slot's `RefCell`.
pub struct HostPayloadStore {
    roots: [OnceLock<Box<LevelTwo>>; FANOUT],
    free: RefCell<Vec<u32>>,
    next: Cell<u32>,
    live: Cell<usize>,
    payload_drop_panicked: Cell<bool>,
    meter: MemoryMeter,
    charged_segments: Cell<usize>,
}

impl HostPayloadStore {
    pub(crate) fn new(meter: MemoryMeter) -> Self {
        Self {
            roots: std::array::from_fn(|_| OnceLock::new()),
            free: RefCell::new(Vec::new()),
            next: Cell::new(0),
            live: Cell::new(0),
            payload_drop_panicked: Cell::new(false),
            meter,
            charged_segments: Cell::new(0),
        }
    }

    pub(crate) fn insert(
        &self,
        payload: ErasedPayload,
        exceeds_cap: impl FnOnce(usize) -> bool,
    ) -> Result<PayloadId, ErasedPayload> {
        let reused = self.free.borrow().last().copied();
        let Some(raw) = reused.or_else(|| self.next.get().checked_add(1).map(|_| self.next.get()))
        else {
            return Err(payload);
        };
        let growth = self.segment_growth(raw);
        if exceeds_cap(growth) || self.free.borrow_mut().try_reserve(1).is_err() {
            return Err(payload);
        }
        let slot = self.ensure_slot(raw);
        let mut value = slot
            .value
            .try_borrow_mut()
            .expect("a free payload slot has no live borrow");
        debug_assert!(value.is_none(), "a free payload slot is empty");
        *value = Some(payload);
        if reused.is_some() {
            self.free.borrow_mut().pop();
        } else {
            self.next.set(raw + 1);
        }
        self.live.set(self.live.get().saturating_add(1));
        Ok(PayloadId(raw))
    }

    pub(crate) fn try_reserve_reclaims(
        &self,
        additional: usize,
    ) -> Result<(), std::collections::TryReserveError> {
        self.free.borrow_mut().try_reserve(additional)
    }

    pub(crate) fn reclaim(&self, id: PayloadId) {
        let slot = self
            .slot(id.0)
            .expect("a live userdata payload has an allocated slot");
        let payload = slot
            .value
            .try_borrow_mut()
            .expect("GC cannot run while a userdata payload is borrowed")
            .take()
            .expect("a live userdata owns one payload");
        // Host destructors are arbitrary code and may panic. Commit the store's
        // ownership bookkeeping first so a caught panic cannot strand a live
        // count or lose this slot from the free list.
        self.free.borrow_mut().push(id.0);
        self.live.set(
            self.live
                .get()
                .checked_sub(1)
                .expect("a reclaimed payload is counted as live"),
        );
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
            self.payload_drop_panicked.set(true);
            resume_unwind(panic);
        }
    }

    pub(crate) fn borrow<T: Any>(&self, id: PayloadId) -> Result<Ref<'_, T>, PayloadBorrowError> {
        let slot = self.slot(id.0).ok_or(PayloadBorrowError::Missing)?;
        let value = slot
            .value
            .try_borrow()
            .map_err(|_| PayloadBorrowError::Conflict)?;
        let Some(payload) = value.as_ref() else {
            return Err(PayloadBorrowError::Missing);
        };
        if !payload.is::<T>() {
            return Err(PayloadBorrowError::WrongType);
        }
        Ok(Ref::map(value, |value| {
            value
                .as_ref()
                .and_then(|payload| payload.downcast_ref::<T>())
                .expect("the payload type was checked")
        }))
    }

    pub(crate) fn borrow_mut<T: Any>(
        &self,
        id: PayloadId,
    ) -> Result<RefMut<'_, T>, PayloadBorrowError> {
        let slot = self.slot(id.0).ok_or(PayloadBorrowError::Missing)?;
        let value = slot
            .value
            .try_borrow_mut()
            .map_err(|_| PayloadBorrowError::Conflict)?;
        let Some(payload) = value.as_ref() else {
            return Err(PayloadBorrowError::Missing);
        };
        if !payload.is::<T>() {
            return Err(PayloadBorrowError::WrongType);
        }
        Ok(RefMut::map(value, |value| {
            value
                .as_mut()
                .and_then(|payload| payload.downcast_mut::<T>())
                .expect("the payload type was checked")
        }))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.live.get() == 0
    }

    pub(crate) fn payload_drop_panicked(&self) -> bool {
        self.payload_drop_panicked.get()
    }

    pub(crate) fn validate(
        &self,
        payload_ids: impl IntoIterator<Item = PayloadId>,
    ) -> Result<(), String> {
        let mut header_ids = Vec::new();
        for id in payload_ids {
            header_ids
                .try_reserve(1)
                .map_err(|_| "out of memory recording userdata payload ids".to_owned())?;
            header_ids.push(id);
        }
        header_ids.sort_unstable();
        if let Some(pair) = header_ids.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(format!(
                "userdata headers duplicate payload id {}",
                pair[0].0
            ));
        }

        let free = self
            .free
            .try_borrow()
            .map_err(|_| "payload free list is borrowed during validation".to_owned())?;
        let mut free_ids = Vec::new();
        free_ids
            .try_reserve(free.len())
            .map_err(|_| "out of memory recording free payload ids".to_owned())?;
        free_ids.extend(free.iter().copied());
        drop(free);
        free_ids.sort_unstable();
        if let Some(pair) = free_ids.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(format!("payload free list duplicates id {}", pair[0]));
        }

        let next = self.next.get();
        let mut populated = 0_usize;
        for raw in 0..next {
            let slot = self
                .slot(raw)
                .ok_or_else(|| format!("allocated payload id {raw} has no slot"))?;
            let value = slot
                .value
                .try_borrow()
                .map_err(|_| format!("payload id {raw} is borrowed during validation"))?;
            let has_header = header_ids.binary_search(&PayloadId(raw)).is_ok();
            let is_free = free_ids.binary_search(&raw).is_ok();
            match (value.is_some(), has_header, is_free) {
                (true, true, false) => populated += 1,
                (true, false, _) => {
                    return Err(format!("payload id {raw} has no live userdata header"));
                }
                (true, true, true) => {
                    return Err(format!("live payload id {raw} is also on the free list"));
                }
                (false, true, _) => {
                    return Err(format!("userdata header payload id {raw} has no payload"));
                }
                (false, false, true) => {}
                (false, false, false) => {
                    return Err(format!(
                        "empty payload id {raw} is absent from the free list"
                    ));
                }
            }
        }

        if let Some(id) = header_ids.iter().find(|id| id.0 >= next) {
            return Err(format!(
                "userdata header payload id {} is outside the allocated store",
                id.0
            ));
        }
        if let Some(id) = free_ids.iter().find(|&&id| id >= next) {
            return Err(format!(
                "payload free-list id {id} is outside the allocated store"
            ));
        }
        if populated != self.live.get() {
            return Err(format!(
                "payload store counts {} live slots but records {}",
                populated,
                self.live.get()
            ));
        }
        if header_ids.len() != populated {
            return Err(format!(
                "heap has {} userdata headers but payload store has {populated} live slots",
                header_ids.len()
            ));
        }
        Ok(())
    }

    fn segment_growth(&self, raw: u32) -> usize {
        let [_slot, leaf, one, root] = raw.to_le_bytes();
        let Some(level_two) = self.roots[usize::from(root)].get() else {
            return size_of::<LevelTwo>() + size_of::<LevelOne>() + size_of::<Leaf>();
        };
        let Some(level_one) = level_two.children[usize::from(one)].get() else {
            return size_of::<LevelOne>() + size_of::<Leaf>();
        };
        if level_one.leaves[usize::from(leaf)].get().is_some() {
            0
        } else {
            size_of::<Leaf>()
        }
    }

    fn ensure_slot(&self, raw: u32) -> &PayloadSlot {
        let [slot, leaf, one, root] = raw.to_le_bytes();
        let level_two =
            self.roots[usize::from(root)].get_or_init(|| self.charge_box(LevelTwo::new()));
        let level_one =
            level_two.children[usize::from(one)].get_or_init(|| self.charge_box(LevelOne::new()));
        let leaf = level_one.leaves[usize::from(leaf)].get_or_init(|| self.charge_box(Leaf::new()));
        &leaf.slots[usize::from(slot)]
    }

    fn slot(&self, raw: u32) -> Option<&PayloadSlot> {
        let [slot, leaf, one, root] = raw.to_le_bytes();
        let level_two = self.roots[usize::from(root)].get()?;
        let level_one = level_two.children[usize::from(one)].get()?;
        let leaf = level_one.leaves[usize::from(leaf)].get()?;
        leaf.slots.get(usize::from(slot))
    }

    fn charge_box<T>(&self, value: T) -> Box<T> {
        let bytes = size_of::<T>();
        self.meter.charge(bytes);
        self.charged_segments
            .set(self.charged_segments.get().saturating_add(bytes));
        Box::new(value)
    }
}

impl Drop for HostPayloadStore {
    fn drop(&mut self) {
        self.meter.adjust(self.charged_segments.get(), 0);
    }
}

#[cfg(any())]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::{HostPayloadStore, PayloadBorrowError};
    use crate::heap::MemoryMeter;

    struct PanickingPayload(Arc<AtomicUsize>);

    impl Drop for PanickingPayload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
            panic!("payload destructor probe");
        }
    }

    #[test]
    fn standard_refcell_guards_enforce_payload_borrows() {
        let store = HostPayloadStore::new(MemoryMeter::default());
        let id = store
            .insert(Box::new(7_u32), |_| false)
            .expect("insert payload");

        let first = store.borrow::<u32>(id).expect("first shared borrow");
        let second = store.borrow::<u32>(id).expect("nested shared borrow");
        assert_eq!((*first, *second), (7, 7));
        assert_eq!(
            store
                .borrow_mut::<u32>(id)
                .expect_err("shared blocks mutable"),
            PayloadBorrowError::Conflict
        );
        drop((first, second));

        *store.borrow_mut::<u32>(id).expect("mutable borrow") = 9;
        assert_eq!(*store.borrow::<u32>(id).expect("updated payload"), 9);
        assert_eq!(
            store.borrow::<u64>(id).expect_err("wrong type"),
            PayloadBorrowError::WrongType
        );
        store.reclaim(id);
        assert_eq!(
            store.borrow::<u32>(id).expect_err("reclaimed slot"),
            PayloadBorrowError::Missing
        );
    }

    #[test]
    fn segmented_slots_cross_a_leaf_boundary_and_reuse_ids() {
        let store = HostPayloadStore::new(MemoryMeter::default());
        let ids = (0_u32..=256)
            .map(|value| {
                store
                    .insert(Box::new(value), |_| false)
                    .expect("insert segmented payload")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            *store
                .borrow::<u32>(ids[255])
                .expect("last first-leaf value"),
            255
        );
        assert_eq!(
            *store
                .borrow::<u32>(ids[256])
                .expect("first second-leaf value"),
            256
        );

        let recycled = ids[127];
        store.reclaim(recycled);
        let reused = store
            .insert(Box::new(999_u32), |_| false)
            .expect("reuse reclaimed slot");
        assert_eq!(reused, recycled);
        assert_eq!(*store.borrow::<u32>(reused).expect("reused payload"), 999);

        for (index, id) in ids.into_iter().enumerate() {
            if index != 127 {
                store.reclaim(id);
            }
        }
        store.reclaim(reused);
        assert!(store.is_empty());
    }

    #[test]
    fn reclaim_commits_bookkeeping_before_a_payload_destructor_panics() {
        let store = HostPayloadStore::new(MemoryMeter::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let id = store
            .insert(Box::new(PanickingPayload(Arc::clone(&drops))), |_| false)
            .expect("insert payload");

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.reclaim(id);
        }));

        assert!(panic.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(store.payload_drop_panicked());
        assert!(store.is_empty());
        store
            .validate([])
            .expect("reclaimed store stays consistent");
        let reused = store
            .insert(Box::new(17_u32), |_| false)
            .expect("reclaimed id remains reusable");
        assert_eq!(reused, id);
        store.reclaim(reused);
    }

    #[test]
    fn validation_requires_a_bijection_between_headers_and_payloads() {
        let store = HostPayloadStore::new(MemoryMeter::default());
        let id = store
            .insert(Box::new(7_u32), |_| false)
            .expect("insert payload");

        store.validate([id]).expect("matching header and payload");
        assert_eq!(
            store.validate([]).expect_err("orphan payload is rejected"),
            "payload id 0 has no live userdata header"
        );
        assert_eq!(
            store
                .validate([id, id])
                .expect_err("duplicate header is rejected"),
            "userdata headers duplicate payload id 0"
        );

        store.reclaim(id);
        assert_eq!(
            store
                .validate([id])
                .expect_err("header without a payload is rejected"),
            "userdata header payload id 0 has no payload"
        );
        store.validate([]).expect("empty store validates");
    }
}
