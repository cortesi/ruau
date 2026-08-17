use std::mem;

use super::{Age, Arena, ArenaEntry, Color, MemoryMeter, StackStore};
use crate::{
    api::{HeapId, RawGc, RawValue, RegistryRef, marker},
    func::{Closure, UpVal},
    gc::GcRef,
    object::{LuaBufferImage, LuaUserdata, ProtoImage},
    snapshot::{self, SnapshotError},
    state::{CoroutineStatus, FrameSnapshot, Thread},
};

/// Runtime `require` cache entry.
pub(super) struct ModuleCacheEntry {
    pub(super) epoch: u64,
    pub(super) reference: RegistryRef,
}

/// Runtime `require` cache identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct ModuleCacheIdentity {
    pub(super) domain: crate::ModuleDomainId,
    pub(super) instance: crate::InstanceKey,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct HeapImage {
    pub(super) objects: ObjectStoreImage,
    pub(super) registry: RegistryImage,
    pub(super) named: Vec<(Vec<u8>, RegistryRefImage)>,
    pub(super) module_cache: Vec<(InstanceKeyImage, u64, RegistryRefImage)>,
    pub(super) next_async_invocation: u64,
    pub(super) gas: Option<u64>,
    pub(super) logical_deadline: Option<u64>,
    pub(super) gas_spent: u64,
    pub(super) quantum: Option<u64>,
    pub(super) quantum_remaining: u64,
    pub(super) string_metatable: Option<RawGc<marker::Table>>,
    pub(super) vector_metatable: Option<RawGc<marker::Table>>,
    pub(super) metamethod_names: [Option<RawGc<marker::Str>>; 18],
    pub(super) rngstate: u64,
    pub(super) gc_rng: u64,
    pub(super) gc_requested: bool,
    pub(super) gc_cycles: u64,
    pub(super) gc_running: bool,
    pub(super) gc_step_progress: usize,
    pub(super) gc_step_ready: bool,
    pub(super) gc_remembered: Vec<GcRef>,
    pub(super) gc_minors_since_major: u32,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(super) struct ObjectStoreImage {
    pub(super) strings: ArenaImage<Vec<u8>>,
    pub(super) tables: ArenaImage<crate::table::LuaTableImage>,
    pub(super) closures: ArenaImage<Closure>,
    pub(super) userdata: ArenaImage<()>,
    pub(super) threads: ArenaImage<ThreadImage>,
    pub(super) buffers: ArenaImage<LuaBufferImage>,
    pub(super) protos: ArenaImage<ProtoImage>,
    pub(super) upvals: ArenaImage<UpVal>,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(super) struct ArenaImage<T> {
    pub(super) entries: Vec<ArenaEntryImage<T>>,
    pub(super) gens: Vec<u32>,
    pub(super) free: Vec<u32>,
    pub(super) young: Vec<u32>,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(super) struct ArenaEntryImage<T> {
    pub(super) color: Color,
    pub(super) age: Age,
    pub(super) value: Option<T>,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(super) struct RegistryImage {
    pub(super) anchors: Vec<RegistrySlotImage>,
    pub(super) free: Vec<u32>,
}

#[derive(Clone, Copy, serde::Deserialize, serde::Serialize)]
pub(super) struct RegistryRefImage {
    slot: u32,
    generation: u32,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(super) struct RegistrySlotImage {
    pub(super) value: RawValue,
    pub(super) generation: u32,
    pub(super) live: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(super) struct ThreadImage {
    pub(super) stacks: Vec<RawValue>,
    pub(super) error_frames: Vec<FrameSnapshot>,
    pub(super) top: u32,
    pub(super) open_upvals: Vec<RawGc<UpVal>>,
    pub(super) id: Option<RawGc<marker::Thread>>,
    pub(super) globals: Option<RawGc<marker::Table>>,
    pub(super) status: CoroutineStatus,
    pub(super) entry: Option<RawGc<marker::Closure>>,
    pub(super) native_depth: u32,
    pub(super) base_native_depth: u32,
    pub(super) death_error: Option<RawValue>,
    pub(super) last_async_invocation: Option<u64>,
}

impl HeapImage {
    pub(crate) fn min_restore_bytes(&self) -> usize {
        self.objects
            .min_restore_bytes()
            .saturating_add(registry_image_min_restore_bytes(&self.registry))
            .saturating_add(vec_min_restore_bytes(&self.named))
            .saturating_add(vec_min_restore_bytes(&self.module_cache))
            .saturating_add(vec_min_restore_bytes(&self.gc_remembered))
    }

    pub(super) fn normalize_gc_metadata(&mut self) -> Result<(), SnapshotError> {
        self.objects.normalize_gc_metadata()?;
        self.gc_remembered.clear();
        Ok(())
    }

    #[cfg(any())]
    pub(crate) fn test_forge_first_proto_host(&mut self, host: crate::object::HostId) -> bool {
        for entry in &mut self.objects.protos.entries {
            let Some(proto) = &mut entry.value else {
                continue;
            };
            proto.host = Some(host);
            return true;
        }
        false
    }

    #[cfg(any())]
    pub(crate) fn test_has_native_proto(&self) -> bool {
        self.objects.protos.entries.iter().any(|entry| {
            entry
                .value
                .as_ref()
                .is_some_and(|proto| proto.native.is_some())
        })
    }

    #[cfg(any())]
    pub(crate) fn test_forge_string_live_slot_as_free(&mut self) -> bool {
        self.objects.strings.test_push_first_live_index_to_free()
    }

    #[cfg(any())]
    pub(crate) fn test_forge_string_duplicate_free_entry(&mut self) -> bool {
        self.objects
            .strings
            .test_make_first_live_slot_duplicate_free()
    }

    #[cfg(any())]
    pub(crate) fn test_forge_string_missing_generation(&mut self) -> bool {
        self.objects.strings.test_remove_last_generation()
    }

    #[cfg(any())]
    pub(crate) fn test_forge_string_out_of_range_free_index(&mut self) -> bool {
        self.objects.strings.test_push_out_of_range_free_index()
    }

    #[cfg(any())]
    pub(crate) fn test_forge_registry_live_slot_as_free(&mut self) -> bool {
        let Some(index) = self.registry.anchors.iter().position(|slot| slot.live) else {
            return false;
        };
        self.registry.free.push(index as u32);
        true
    }

    #[cfg(any())]
    pub(crate) fn test_forge_registry_duplicate_free_entry(&mut self) -> bool {
        let Some(index) = self.registry.free.first().copied() else {
            let Some(index) = self.registry.anchors.iter().position(|slot| !slot.live) else {
                return false;
            };
            self.registry.free.push(index as u32);
            self.registry.free.push(index as u32);
            return true;
        };
        self.registry.free.push(index);
        true
    }

    #[cfg(any())]
    pub(crate) fn test_forge_string_missing_young_entry(&mut self) -> bool {
        self.objects.strings.test_make_first_live_young_missing()
    }

    #[cfg(any())]
    pub(crate) fn test_forge_gc_metadata_for_normalization(&mut self) -> bool {
        self.gc_remembered.clear();
        self.objects.test_forge_gc_metadata_for_normalization()
    }
}

impl ObjectStoreImage {
    fn normalize_gc_metadata(&mut self) -> Result<(), SnapshotError> {
        self.strings.normalize_gc_metadata()?;
        self.tables.normalize_gc_metadata()?;
        self.closures.normalize_gc_metadata()?;
        self.userdata.normalize_gc_metadata()?;
        self.threads.normalize_gc_metadata()?;
        self.buffers.normalize_gc_metadata()?;
        self.protos.normalize_gc_metadata()?;
        self.upvals.normalize_gc_metadata()?;
        Ok(())
    }

    fn min_restore_bytes(&self) -> usize {
        arena_image_min_restore_bytes(&self.strings)
            .saturating_add(arena_values_min_restore_bytes(&self.strings, |bytes| {
                bytes.len()
            }))
            .saturating_add(arena_image_min_restore_bytes(&self.tables))
            .saturating_add(arena_values_min_restore_bytes(
                &self.tables,
                table_image_min_restore_bytes,
            ))
            .saturating_add(arena_image_min_restore_bytes(&self.closures))
            .saturating_add(arena_values_min_restore_bytes(
                &self.closures,
                Closure::gc_footprint,
            ))
            .saturating_add(arena_image_min_restore_bytes(&self.userdata))
            .saturating_add(arena_image_min_restore_bytes(&self.threads))
            .saturating_add(arena_values_min_restore_bytes(
                &self.threads,
                thread_image_min_restore_bytes,
            ))
            .saturating_add(arena_image_min_restore_bytes(&self.buffers))
            .saturating_add(arena_values_min_restore_bytes(&self.buffers, |buffer| {
                buffer.data.len()
            }))
            .saturating_add(arena_image_min_restore_bytes(&self.protos))
            .saturating_add(arena_values_min_restore_bytes(
                &self.protos,
                proto_image_min_restore_bytes,
            ))
            .saturating_add(arena_image_min_restore_bytes(&self.upvals))
    }

    #[cfg(any())]
    fn test_forge_gc_metadata_for_normalization(&mut self) -> bool {
        let mut forged = false;
        forged |= self.strings.test_forge_gc_metadata_for_normalization();
        forged |= self.tables.test_forge_gc_metadata_for_normalization();
        forged |= self.closures.test_forge_gc_metadata_for_normalization();
        forged |= self.userdata.test_forge_gc_metadata_for_normalization();
        forged |= self.threads.test_forge_gc_metadata_for_normalization();
        forged |= self.buffers.test_forge_gc_metadata_for_normalization();
        forged |= self.protos.test_forge_gc_metadata_for_normalization();
        forged |= self.upvals.test_forge_gc_metadata_for_normalization();
        forged
    }
}

fn arena_image_min_restore_bytes<T>(arena: &ArenaImage<T>) -> usize {
    vec_min_restore_bytes(&arena.entries)
        .saturating_add(vec_min_restore_bytes(&arena.gens))
        .saturating_add(vec_min_restore_bytes(&arena.free))
        .saturating_add(vec_min_restore_bytes(&arena.young))
}

fn arena_values_min_restore_bytes<T>(
    arena: &ArenaImage<T>,
    mut value_bytes: impl FnMut(&T) -> usize,
) -> usize {
    arena
        .entries
        .iter()
        .filter_map(|entry| entry.value.as_ref())
        .fold(0usize, |total, value| {
            total.saturating_add(value_bytes(value))
        })
}

fn registry_image_min_restore_bytes(registry: &RegistryImage) -> usize {
    vec_min_restore_bytes(&registry.anchors).saturating_add(vec_min_restore_bytes(&registry.free))
}

fn table_image_min_restore_bytes(table: &crate::table::LuaTableImage) -> usize {
    vec_min_restore_bytes(&table.array).saturating_add(vec_min_restore_bytes(&table.hash))
}

fn thread_image_min_restore_bytes(thread: &ThreadImage) -> usize {
    vec_min_restore_bytes(&thread.stacks)
        .saturating_add(vec_min_restore_bytes(&thread.error_frames))
        .saturating_add(vec_min_restore_bytes(&thread.open_upvals))
}

fn proto_image_min_restore_bytes(proto: &ProtoImage) -> usize {
    vec_min_restore_bytes(&proto.code)
        .saturating_add(vec_min_restore_bytes(&proto.jump_targets))
        .saturating_add(vec_min_restore_bytes(&proto.constants))
        .saturating_add(vec_min_restore_bytes(&proto.import_values))
        .saturating_add(vec_min_restore_bytes(&proto.child_protos))
        .saturating_add(vec_min_restore_bytes(&proto.lines))
        .saturating_add(vec_min_restore_bytes(&proto.coverage_hits))
        .saturating_add(proto.module_id.as_ref().map_or(0, Vec::len))
}

fn vec_min_restore_bytes<T>(values: &[T]) -> usize {
    values.len().saturating_mul(mem::size_of::<T>())
}

impl RegistryRefImage {
    pub(super) fn from_ref(reference: &RegistryRef) -> Self {
        Self {
            slot: reference.slot(),
            generation: reference.generation(),
        }
    }

    pub(super) fn into_ref(self, heap: HeapId) -> RegistryRef {
        RegistryRef::from_parts(self.slot, self.generation, heap)
    }
}

impl<T> ArenaImage<T> {
    fn normalize_gc_metadata(&mut self) -> Result<(), SnapshotError> {
        self.young.clear();
        for (index, entry) in self.entries.iter_mut().enumerate() {
            entry.color = Color::White;
            if entry.value.is_some() {
                entry.age = Age::Young;
                let index = u32::try_from(index)
                    .map_err(|_| SnapshotError::Invalid("arena index out of range"))?;
                self.young.push(index);
            } else {
                entry.age = Age::Old;
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), SnapshotError> {
        let entries_len = self.entries.len();
        if self.gens.len() < entries_len {
            return Err(SnapshotError::Invalid("arena generation missing"));
        }

        let mut free_seen = vec![false; entries_len];
        for &index in &self.free {
            let index = index as usize;
            let Some(entry) = self.entries.get(index) else {
                return Err(SnapshotError::Invalid("arena free index out of range"));
            };
            if free_seen[index] {
                return Err(SnapshotError::Invalid("arena duplicate free index"));
            }
            if entry.value.is_some() {
                return Err(SnapshotError::Invalid(
                    "arena free index references live slot",
                ));
            }
            free_seen[index] = true;
        }
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.value.is_none() && !free_seen[index] {
                return Err(SnapshotError::Invalid(
                    "arena vacant slot missing from free list",
                ));
            }
        }

        let mut young_seen = vec![false; entries_len];
        for &index in &self.young {
            let index = index as usize;
            let Some(entry) = self.entries.get(index) else {
                return Err(SnapshotError::Invalid("arena young index out of range"));
            };
            if young_seen[index] {
                return Err(SnapshotError::Invalid("arena duplicate young index"));
            }
            if entry.value.is_none() {
                return Err(SnapshotError::Invalid(
                    "arena young index references free slot",
                ));
            }
            if entry.age != Age::Young {
                return Err(SnapshotError::Invalid(
                    "arena young index references non-young slot",
                ));
            }
            young_seen[index] = true;
        }
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.value.is_some() && entry.age == Age::Young && !young_seen[index] {
                return Err(SnapshotError::Invalid(
                    "arena young slot missing from young list",
                ));
            }
        }

        Ok(())
    }

    pub(super) fn rebrand_each(&mut self, mut rebrand: impl FnMut(&mut T)) {
        for entry in &mut self.entries {
            if let Some(value) = &mut entry.value {
                rebrand(value);
            }
        }
    }

    pub(super) fn restore_arena<U>(
        self,
        meter: MemoryMeter,
        mut map: impl FnMut(T) -> U,
    ) -> Result<Arena<U>, SnapshotError> {
        self.validate()?;
        Ok(Arena::from_snapshot_entries(
            self.entries
                .into_iter()
                .map(|entry| ArenaEntry {
                    color: entry.color,
                    age: entry.age,
                    value: entry.value.map(&mut map),
                })
                .collect(),
            self.gens,
            self.free,
            self.young,
            meter,
        ))
    }

    pub(super) fn try_restore_arena<U>(
        self,
        meter: MemoryMeter,
        mut map: impl FnMut(T) -> Result<U, SnapshotError>,
    ) -> Result<Arena<U>, SnapshotError> {
        self.validate()?;
        let entries = self
            .entries
            .into_iter()
            .map(|entry| {
                Ok(ArenaEntry {
                    color: entry.color,
                    age: entry.age,
                    value: entry.value.map(&mut map).transpose()?,
                })
            })
            .collect::<Result<Vec<_>, SnapshotError>>()?;
        Ok(Arena::from_snapshot_entries(
            entries, self.gens, self.free, self.young, meter,
        ))
    }

    #[cfg(any())]
    fn first_live_index(&self) -> Option<usize> {
        self.entries.iter().position(|entry| entry.value.is_some())
    }

    #[cfg(any())]
    fn test_push_first_live_index_to_free(&mut self) -> bool {
        let Some(index) = self.first_live_index() else {
            return false;
        };
        self.free.push(index as u32);
        true
    }

    #[cfg(any())]
    fn test_make_first_live_slot_duplicate_free(&mut self) -> bool {
        let Some(index) = self.first_live_index() else {
            return false;
        };
        self.entries[index].value = None;
        self.free.push(index as u32);
        self.free.push(index as u32);
        true
    }

    #[cfg(any())]
    fn test_remove_last_generation(&mut self) -> bool {
        if self.entries.is_empty() || self.gens.len() < self.entries.len() {
            return false;
        }
        self.gens.truncate(self.entries.len() - 1);
        true
    }

    #[cfg(any())]
    fn test_push_out_of_range_free_index(&mut self) -> bool {
        let Ok(index) = u32::try_from(self.entries.len()) else {
            return false;
        };
        self.free.push(index);
        true
    }

    #[cfg(any())]
    fn test_make_first_live_young_missing(&mut self) -> bool {
        let Some(index) = self.first_live_index() else {
            return false;
        };
        self.entries[index].age = Age::Young;
        self.young.retain(|&young| young as usize != index);
        true
    }

    #[cfg(any())]
    fn test_forge_gc_metadata_for_normalization(&mut self) -> bool {
        self.young.clear();
        let mut forged = false;
        for entry in &mut self.entries {
            if entry.value.is_none() {
                continue;
            }
            entry.color = if forged { Color::Gray } else { Color::Black };
            entry.age = Age::Old;
            forged = true;
        }
        forged
    }
}

impl ThreadImage {
    pub(super) fn from_thread(thread: &Thread) -> Result<Self, SnapshotError> {
        if !thread.call_stack.is_empty() {
            return Err(SnapshotError::Unsupported(
                "suspended coroutine call stacks are not in the prototype codec",
            ));
        }
        if thread.resume_slot.is_some() {
            return Err(SnapshotError::Unsupported(
                "yield resume slots are not in the prototype codec",
            ));
        }
        if thread.resumer.is_some() {
            return Err(SnapshotError::NotQuiescent("thread has an active resumer"));
        }
        if thread.captured_traceback.is_some() {
            return Err(SnapshotError::Unsupported(
                "captured traceback state is not in the prototype codec",
            ));
        }
        Ok(Self {
            stacks: thread.stacks.snapshot_slots(),
            error_frames: thread.error_frames.clone(),
            top: thread.top,
            open_upvals: thread.open_upvals.clone(),
            id: thread.id,
            globals: thread.globals,
            status: thread.status,
            entry: thread.entry,
            native_depth: thread.native_depth,
            base_native_depth: thread.base_native_depth,
            death_error: thread.death_error,
            last_async_invocation: thread.last_async_invocation,
        })
    }

    pub(super) fn into_thread(self, meter: MemoryMeter) -> Thread {
        Thread {
            stacks: StackStore::from_snapshot_slots(self.stacks, meter),
            call_stack: Vec::new(),
            error_frames: self.error_frames,
            top: self.top,
            open_upvals: self.open_upvals,
            id: self.id,
            globals: self.globals,
            status: self.status,
            entry: self.entry,
            resume_slot: None,
            native_depth: self.native_depth,
            base_native_depth: self.base_native_depth,
            death_error: self.death_error,
            resumer: None,
            last_async_invocation: self.last_async_invocation,
            captured_traceback: None,
        }
    }
}

fn rebrand_opt<T>(value: Option<RawGc<T>>, heap: HeapId) -> Option<RawGc<T>> {
    value.map(|handle| snapshot::rebrand_raw(handle, heap))
}

fn rebrand_values(values: &mut [RawValue], heap: HeapId) {
    for value in values {
        *value = snapshot::rebrand_value(*value, heap);
    }
}

fn rebrand_table(image: &mut crate::table::LuaTableImage, heap: HeapId) {
    rebrand_values(&mut image.array, heap);
    for slot in &mut image.hash {
        slot.key_value = snapshot::rebrand_value(slot.key_value, heap);
        if let Some(value) = slot.value {
            slot.value = Some(snapshot::rebrand_value(value, heap));
        }
    }
    image.metatable = rebrand_opt(image.metatable, heap);
}

fn rebrand_closure(closure: &mut Closure, heap: HeapId) {
    closure.proto = snapshot::rebrand_raw(closure.proto, heap);
    closure.env = rebrand_opt(closure.env, heap);
    for upval in &mut closure.upvals {
        *upval = snapshot::rebrand_raw(*upval, heap);
    }
}

fn rebrand_upval(upval: &mut UpVal, heap: HeapId) {
    match upval {
        UpVal::Open { thread, .. } => *thread = snapshot::rebrand_raw(*thread, heap),
        UpVal::Closed(value) => *value = snapshot::rebrand_value(*value, heap),
    }
}

fn rebrand_proto(image: &mut ProtoImage, heap: HeapId) {
    for constant in &mut image.constants {
        rebrand_constant(constant, heap);
    }
    for value in &mut image.import_values {
        if let Some(raw) = *value {
            *value = Some(snapshot::rebrand_value(raw, heap));
        }
    }
    for proto in &mut image.child_protos {
        *proto = snapshot::rebrand_raw(*proto, heap);
    }
    image.debug_name = rebrand_opt(image.debug_name, heap);
    image.source = rebrand_opt(image.source, heap);
}

fn rebrand_constant(constant: &mut crate::object::RuntimeConstant, heap: HeapId) {
    match constant {
        crate::object::RuntimeConstant::Value(value) => {
            *value = snapshot::rebrand_value(*value, heap);
        }
        crate::object::RuntimeConstant::Import(_) => {}
        crate::object::RuntimeConstant::Table(shape) => {
            for (key, value) in &mut shape.entries {
                *key = snapshot::rebrand_value(*key, heap);
                *value = snapshot::rebrand_value(*value, heap);
            }
        }
        crate::object::RuntimeConstant::Proto(proto) => {
            *proto = snapshot::rebrand_raw(*proto, heap);
        }
    }
}

fn rebrand_thread_image(image: &mut ThreadImage, heap: HeapId) {
    rebrand_values(&mut image.stacks, heap);
    for frame in &mut image.error_frames {
        frame.closure = snapshot::rebrand_raw(frame.closure, heap);
    }
    for upval in &mut image.open_upvals {
        *upval = snapshot::rebrand_raw(*upval, heap);
    }
    image.id = rebrand_opt(image.id, heap);
    image.globals = rebrand_opt(image.globals, heap);
    image.entry = rebrand_opt(image.entry, heap);
    if let Some(value) = image.death_error {
        image.death_error = Some(snapshot::rebrand_value(value, heap));
    }
}

pub(super) fn snapshot_closure(closure: &Closure) -> Closure {
    Closure {
        proto: closure.proto,
        env: closure.env,
        upvals: closure.upvals.clone(),
    }
}

pub(super) fn snapshot_upval(upval: &UpVal) -> UpVal {
    match upval {
        UpVal::Open { thread, slot } => UpVal::Open {
            thread: *thread,
            slot: *slot,
        },
        UpVal::Closed(value) => UpVal::Closed(*value),
    }
}

pub(super) fn restore_empty_userdata_arena(
    image: ArenaImage<()>,
    meter: MemoryMeter,
) -> Result<Arena<LuaUserdata>, SnapshotError> {
    image.validate()?;
    let entries = image
        .entries
        .into_iter()
        .map(|entry| {
            if entry.value.is_some() {
                return Err(SnapshotError::Unsupported(
                    "live host userdata is not in the prototype codec",
                ));
            }
            Ok(ArenaEntry {
                color: entry.color,
                age: entry.age,
                value: None,
            })
        })
        .collect::<Result<Vec<_>, SnapshotError>>()?;
    Ok(Arena::from_snapshot_entries(
        entries,
        image.gens,
        image.free,
        image.young,
        meter,
    ))
}

pub(super) fn rebrand_heap_image(image: &mut HeapImage, heap: HeapId) {
    image
        .objects
        .tables
        .rebrand_each(|table| rebrand_table(table, heap));
    image
        .objects
        .closures
        .rebrand_each(|closure| rebrand_closure(closure, heap));
    image
        .objects
        .threads
        .rebrand_each(|thread| rebrand_thread_image(thread, heap));
    image
        .objects
        .protos
        .rebrand_each(|proto| rebrand_proto(proto, heap));
    image
        .objects
        .upvals
        .rebrand_each(|upval| rebrand_upval(upval, heap));
    for slot in &mut image.registry.anchors {
        slot.value = snapshot::rebrand_value(slot.value, heap);
    }
    image.string_metatable = rebrand_opt(image.string_metatable, heap);
    image.vector_metatable = rebrand_opt(image.vector_metatable, heap);
    for name in &mut image.metamethod_names {
        *name = rebrand_opt(*name, heap);
    }
}

/// Runtime in-flight `require` key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ModuleCacheKey {
    domain: crate::ModuleDomainId,
    instance: crate::InstanceKey,
    epoch: u64,
}

impl ModuleCacheKey {
    pub(crate) fn new(
        domain: crate::ModuleDomainId,
        instance: crate::InstanceKey,
        epoch: u64,
    ) -> Self {
        Self {
            domain,
            instance,
            epoch,
        }
    }

    pub(crate) const fn domain(&self) -> crate::ModuleDomainId {
        self.domain
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(super) struct InstanceKeyImage {
    id: Vec<u8>,
    requester: Option<Vec<u8>>,
}

impl From<&crate::InstanceKey> for InstanceKeyImage {
    fn from(key: &crate::InstanceKey) -> Self {
        Self {
            id: key.id().as_bytes().to_vec(),
            requester: key
                .requester()
                .map(|requester| requester.as_bytes().to_vec()),
        }
    }
}

impl From<InstanceKeyImage> for crate::InstanceKey {
    fn from(image: InstanceKeyImage) -> Self {
        Self::new(
            crate::ModuleId::from(image.id),
            image.requester.map(crate::ModuleId::from),
        )
    }
}
