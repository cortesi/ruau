use ruau_vm_api::{HeapId, RawValue, RegistryRef};

use super::{AccountedVec, MemoryMeter, RegistryImage, RegistrySlotImage};

struct RegistrySlot {
    value: RawValue,
    generation: u32,
    token: Option<RegistryRef>,
}

/// The `lua_ref` registry: host-owned anchors that keep Lua values rooted across an
/// await and across collection. A pinned value is a GC root; a loaded
/// module pins its main closure here so a host holding it across a collection does not
/// have it swept.
pub struct Registry {
    anchors: AccountedVec<RegistrySlot>,
    free: Vec<u32>,
}

impl Registry {
    pub(super) fn with_meter(meter: MemoryMeter) -> Self {
        Self {
            anchors: AccountedVec::with_meter(meter),
            free: Vec::new(),
        }
    }

    /// Pins `value` as a GC root, returning its unforgeable registry token.
    /// Reuses a freed slot when one is available.
    ///
    /// # Errors
    /// Returns `None` if growing the anchor store would exceed memory.
    pub(crate) fn pin(&mut self, value: RawValue, heap: HeapId) -> Option<RegistryRef> {
        if let Some(index) = self.free.pop() {
            let slot = self.anchors.get_mut(index as usize)?;
            let reference = RegistryRef::from_parts(index, slot.generation, heap);
            slot.value = value;
            slot.token = Some(reference.clone());
            Some(reference)
        } else {
            let index = u32::try_from(
                self.anchors
                    .try_push(RegistrySlot {
                        value,
                        generation: 0,
                        token: None,
                    })
                    .ok()?,
            )
            .ok()?;
            let reference = RegistryRef::from_parts(index, 0, heap);
            self.anchors.get_mut(index as usize)?.token = Some(reference.clone());
            Some(reference)
        }
    }

    /// Returns the pinned value if `reference` is the exact live token for its slot.
    ///
    /// Liveness is the slot's token, not its value: a freed slot holds `token:
    /// None` (and a `Nil` value so it roots nothing), while `Nil` itself is a
    /// pinnable value — the generic value stash pins any value kind, immediates
    /// included.
    #[must_use]
    pub(crate) fn get(&self, reference: &RegistryRef) -> Option<RawValue> {
        let slot = self.anchors.get(reference.slot() as usize)?;
        (slot.generation == reference.generation()
            && slot.token.as_ref().is_some_and(|token| token == reference))
        .then_some(slot.value)
    }

    /// Releases the pinned slot for `reference`, freeing it for reuse. Idempotent:
    /// an already-free, stale-generation, or non-identical token is a no-op, so a benign
    /// double-release cannot corrupt the store or free a reused slot.
    pub(crate) fn unpin(&mut self, reference: &RegistryRef) {
        let Some(slot) = self.anchors.get_mut(reference.slot() as usize) else {
            return;
        };
        if slot.generation == reference.generation()
            && slot.token.as_ref().is_some_and(|token| token == reference)
        {
            slot.value = RawValue::Nil;
            slot.token = None;
            slot.generation = slot.generation.wrapping_add(1);
            self.free.push(reference.slot());
        }
    }

    /// GC: the live pinned values, each a root the collector marks. Freed slots hold
    /// `Nil` and are filtered out; a deliberately pinned `Nil` is filtered too,
    /// which is correct — it contributes no root.
    pub(crate) fn gc_anchors(&self) -> impl Iterator<Item = RawValue> + '_ {
        (0..self.anchors.len())
            .filter_map(|i| self.anchors.get(i).map(|slot| slot.value))
            .filter(|value| *value != RawValue::Nil)
    }

    pub(super) fn snapshot_image(&self) -> RegistryImage {
        RegistryImage {
            anchors: self
                .anchors
                .inner
                .iter()
                .map(|slot| RegistrySlotImage {
                    value: slot.value,
                    generation: slot.generation,
                    live: slot.token.is_some(),
                })
                .collect(),
            free: self.free.clone(),
        }
    }

    pub(super) fn from_snapshot_image(
        image: RegistryImage,
        meter: MemoryMeter,
        heap: HeapId,
    ) -> Self {
        let anchors = image
            .anchors
            .into_iter()
            .enumerate()
            .map(|(slot, image)| {
                let token = image
                    .live
                    .then(|| RegistryRef::from_parts(slot as u32, image.generation, heap));
                RegistrySlot {
                    value: image.value,
                    generation: image.generation,
                    token,
                }
            })
            .collect::<Vec<_>>();
        Self {
            anchors: AccountedVec::from_vec(anchors, meter),
            free: image.free,
        }
    }
}
