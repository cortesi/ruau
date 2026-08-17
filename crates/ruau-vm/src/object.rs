//! Loaded prototypes and the object-layout pieces shared across heap objects
//! (port `lobject.h`).
//!
//! A [`Proto`] is the loaded form of a `Proto`: its instructions, a
//! resolved constant table, and references to its child prototypes. Constants
//! that cannot resolve at load time (imports) stay deferred; table and closure
//! constants become templates the interpreter instantiates.

use std::mem::size_of;

use ruau_bytecode::Instruction;

use crate::{
    api::{RawGc, RawValue, marker},
    builtins::Builtin,
    heap::MemoryMeter,
    snapshot::SnapshotError,
};

/// Index of a registered host function in the heap's host registry. A closure
/// over a [`Proto::host`] prototype dispatches to it through the host-call ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct HostId(pub usize);

/// A loaded function prototype, or — when [`native`](Self::native) or
/// [`host`](Self::host) is set — a marker prototype behind an engine builtin or
/// a host function (neither carries bytecode).
pub struct Proto {
    /// The engine builtin this prototype stands in for, if any. A closure over a
    /// native prototype is dispatched directly, without a register frame.
    pub native: Option<Builtin>,
    /// The host function this prototype stands in for, if any. Like `native`, it
    /// carries no bytecode and dispatches without a register frame — but through
    /// the host-call ABI (`HostFunction::call`), which may suspend.
    pub host: Option<HostId>,
    /// Register window size.
    pub max_stack_size: u8,
    /// Fixed parameter count.
    pub num_params: u8,
    /// Number of upvalues the closure captures.
    pub num_upvalues: u8,
    /// Whether the function is variadic.
    pub is_vararg: bool,
    /// The instruction stream, decoded at dispatch time.
    code: Vec<Instruction>,
    /// Resolved absolute jump target (instruction index) per instruction, or
    /// `u32::MAX` for a non-branch instruction or an out-of-range target.
    /// Precomputed at load so a branch is an array index, not a per-jump rescan
    /// of the whole instruction stream.
    jump_targets: Vec<u32>,
    /// The resolved constant table.
    constants: Vec<RuntimeConstant>,
    /// Per-import-site cache of resolved `GETIMPORT` values, keyed by the
    /// instruction's constant index. Populated only while the active
    /// environment is the safeenv-frozen globals (imports are immutable
    /// there); stores go through the generational write barrier and the
    /// values are GC-traced like constants.
    import_values: Vec<Option<RawValue>>,
    /// Child prototypes, by handle, in wire order.
    child_protos: Vec<RawGc<Self>>,
    /// Source line where the function is defined.
    pub line_defined: u32,
    /// Optional debug name for `debug.info` and traceback naming.
    pub debug_name: Option<RawGc<marker::Str>>,
    /// The absolute source line of each instruction, decoded from the chunk's
    /// line info at load. Empty when the chunk carries no line info; otherwise
    /// indexed by program counter, so a runtime error reports its source line.
    lines: Vec<u32>,
    /// Per-instruction coverage hits. Empty unless the proto contains coverage
    /// instrumentation; otherwise indexed by program counter.
    coverage_hits: Vec<u32>,
    /// The chunk name shared by every prototype in the module — the prefix of a
    /// runtime error's `source:line:` location. Bound at load; `None` on a native
    /// builtin prototype.
    pub source: Option<RawGc<marker::Str>>,
    /// Canonical module id for prototypes loaded from [`crate::SourceProvider`].
    ///
    /// Debug source strings carry Luau chunk-name markers (`=`/`@`) and cannot
    /// unambiguously represent every canonical id. Runtime `require` uses this
    /// field as the requester identity for nested relative imports.
    pub(crate) module_id: Option<crate::ModuleId>,
    /// The exact byte footprint charged for the owned buffers above.
    charged_footprint: usize,
    /// Whether the loader has populated this prototype's immutable buffers.
    populated: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct ProtoImage {
    pub native: Option<Builtin>,
    pub host: Option<HostId>,
    pub max_stack_size: u8,
    pub num_params: u8,
    pub num_upvalues: u8,
    pub is_vararg: bool,
    pub code: Vec<Instruction>,
    pub jump_targets: Vec<u32>,
    pub constants: Vec<RuntimeConstant>,
    pub import_values: Vec<Option<RawValue>>,
    pub child_protos: Vec<RawGc<Proto>>,
    pub line_defined: u32,
    pub debug_name: Option<RawGc<marker::Str>>,
    pub lines: Vec<u32>,
    pub coverage_hits: Vec<u32>,
    pub source: Option<RawGc<marker::Str>>,
    pub module_id: Option<Vec<u8>>,
    pub populated: bool,
}

impl Proto {
    /// A first-pass bytecode prototype. The loader later fills its immutable
    /// buffers exactly once through [`populate`](Self::populate).
    #[must_use]
    pub(crate) fn placeholder(
        max_stack_size: u8,
        num_params: u8,
        num_upvalues: u8,
        is_vararg: bool,
        line_defined: u32,
    ) -> Self {
        Self {
            native: None,
            host: None,
            max_stack_size,
            num_params,
            num_upvalues,
            is_vararg,
            code: Vec::new(),
            jump_targets: Vec::new(),
            constants: Vec::new(),
            import_values: Vec::new(),
            child_protos: Vec::new(),
            line_defined,
            debug_name: None,
            lines: Vec::new(),
            coverage_hits: Vec::new(),
            source: None,
            module_id: None,
            charged_footprint: 0,
            populated: false,
        }
    }

    /// The marker prototype for an engine `builtin`: no bytecode, dispatched
    /// directly by `precall`.
    #[must_use]
    pub fn native(builtin: Builtin) -> Self {
        Self {
            native: Some(builtin),
            host: None,
            max_stack_size: 0,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: true,
            code: Vec::new(),
            jump_targets: Vec::new(),
            constants: Vec::new(),
            import_values: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 0,
            debug_name: None,
            lines: Vec::new(),
            coverage_hits: Vec::new(),
            source: None,
            module_id: None,
            charged_footprint: 0,
            populated: true,
        }
    }

    /// The heap footprint of this prototype's owned buffers (reserved capacity),
    /// charged to the memory meter at allocation since the arena counts only the
    /// `Proto` struct header. A native/host marker proto has empty buffers, so
    /// this is zero for them.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.charged_footprint
    }

    /// Populates a first-pass placeholder with its resolved bytecode buffers.
    ///
    /// Returns the byte footprint that must be charged to the heap. A prototype
    /// can only be populated once; after this, the buffers are only observable
    /// through read-only accessors.
    pub(crate) fn populate(&mut self, buffers: ProtoBuffers) -> Option<usize> {
        if self.populated || self.native.is_some() || self.host.is_some() {
            return None;
        }
        let footprint = buffers.footprint();
        self.code = buffers.code;
        self.jump_targets = buffers.jump_targets;
        self.constants = buffers.constants;
        self.child_protos = buffers.child_protos;
        self.lines = buffers.lines;
        self.coverage_hits = buffers.coverage_hits;
        self.source = Some(buffers.source);
        self.module_id = buffers.module_id;
        self.debug_name = buffers.debug_name;
        self.charged_footprint = footprint;
        self.populated = true;
        Some(footprint)
    }

    #[must_use]
    pub(crate) fn instruction(&self, pc: usize) -> Option<Instruction> {
        // POD copy: the dispatch fetch must never allocate.
        self.code.get(pc).copied()
    }

    /// The cached resolved value for the `GETIMPORT` whose constant index is
    /// `index`, when one was stored.
    #[must_use]
    pub(crate) fn cached_import(&self, index: u32) -> Option<RawValue> {
        self.import_values.get(index as usize).copied().flatten()
    }

    /// Caches a resolved `GETIMPORT` value for `index`. The caller is
    /// responsible for the write barrier (the proto may be old, the value
    /// young).
    pub(crate) fn cache_import(&mut self, index: u32, value: RawValue) {
        let index = index as usize;
        if self.import_values.len() <= index {
            if index >= self.constants.len() {
                return;
            }
            self.import_values.resize(self.constants.len(), None);
        }
        self.import_values[index] = Some(value);
    }

    #[must_use]
    pub(crate) fn jump_target(&self, pc: usize) -> Option<u32> {
        self.jump_targets.get(pc).copied()
    }

    #[must_use]
    pub(crate) fn constant(&self, idx: u32) -> Option<&RuntimeConstant> {
        self.constants.get(idx as usize)
    }

    #[must_use]
    pub(crate) fn child_proto(&self, idx: u32) -> Option<RawGc<Self>> {
        self.child_protos.get(idx as usize).copied()
    }

    #[must_use]
    pub(crate) fn child_protos(&self) -> &[RawGc<Self>] {
        &self.child_protos
    }

    #[must_use]
    pub(crate) fn line(&self, pc: usize) -> Option<u32> {
        self.lines.get(pc).copied()
    }

    pub(crate) fn hit_coverage(&mut self, pc: usize) {
        if let Some(hits) = self.coverage_hits.get_mut(pc) {
            *hits = hits.saturating_add(1);
        }
    }

    pub(crate) fn coverage(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.coverage_hits
            .iter()
            .enumerate()
            .filter_map(|(pc, hits)| {
                (self.instruction(pc)?.opcode == ruau_bytecode::opcodes::Opcode::Coverage)
                    .then_some((self.line(pc).unwrap_or(0), *hits))
            })
    }

    #[must_use]
    pub(crate) fn has_line_info(&self) -> bool {
        !self.lines.is_empty()
    }

    /// GC: appends the prototype's outgoing handles — the debug `source` string,
    /// nested prototypes, and any handle-bearing constants (preloaded-table shapes
    /// and constant prototypes) — to the work list. Numeric/string constants whose
    /// strings are interned are reached as values here.
    pub(crate) fn gc_trace<V: crate::gc::GcVisit>(
        &self,
        v: &mut V,
    ) -> Result<(), crate::gc::GcAbort> {
        use crate::gc::GcRef;
        if let Some(source) = self.source {
            v.visit(GcRef::Str(source.index()), source.generation())?;
        }
        if let Some(debug_name) = self.debug_name {
            v.visit(GcRef::Str(debug_name.index()), debug_name.generation())?;
        }
        for child in &self.child_protos {
            v.visit(GcRef::Proto(child.index()), child.generation())?;
        }
        for cached in self.import_values.iter().flatten() {
            if let Some((child, generation)) = GcRef::from_value_gen(*cached) {
                v.visit(child, generation)?;
            }
        }
        for constant in &self.constants {
            match constant {
                RuntimeConstant::Value(value) => {
                    if let Some((child, generation)) = GcRef::from_value_gen(*value) {
                        v.visit(child, generation)?;
                    }
                }
                RuntimeConstant::Proto(proto) => {
                    v.visit(GcRef::Proto(proto.index()), proto.generation())?;
                }
                RuntimeConstant::Table(shape) => {
                    for (key, value) in &shape.entries {
                        if let Some((child, generation)) = GcRef::from_value_gen(*key) {
                            v.visit(child, generation)?;
                        }
                        if let Some((child, generation)) = GcRef::from_value_gen(*value) {
                            v.visit(child, generation)?;
                        }
                    }
                }
                RuntimeConstant::Import(_) => {}
            }
        }
        Ok(())
    }

    /// The marker prototype for a registered host function: no bytecode,
    /// dispatched by `precall` through the host-call ABI.
    #[must_use]
    pub fn host(id: HostId) -> Self {
        Self {
            native: None,
            host: Some(id),
            max_stack_size: 0,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: true,
            code: Vec::new(),
            jump_targets: Vec::new(),
            constants: Vec::new(),
            import_values: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 0,
            debug_name: None,
            lines: Vec::new(),
            coverage_hits: Vec::new(),
            source: None,
            module_id: None,
            charged_footprint: 0,
            populated: true,
        }
    }

    pub(crate) fn snapshot_image(&self) -> ProtoImage {
        ProtoImage {
            native: self.native,
            host: self.host,
            max_stack_size: self.max_stack_size,
            num_params: self.num_params,
            num_upvalues: self.num_upvalues,
            is_vararg: self.is_vararg,
            code: self.code.clone(),
            jump_targets: self.jump_targets.clone(),
            constants: self.constants.clone(),
            import_values: self.import_values.clone(),
            child_protos: self.child_protos.clone(),
            line_defined: self.line_defined,
            debug_name: self.debug_name,
            lines: self.lines.clone(),
            coverage_hits: self.coverage_hits.clone(),
            source: self.source,
            module_id: self.module_id.as_ref().map(|id| id.as_bytes().to_vec()),
            populated: self.populated,
        }
    }

    pub(crate) fn from_snapshot_image(image: ProtoImage) -> Result<Self, SnapshotError> {
        if image.host.is_some() {
            return Err(SnapshotError::Invalid(
                "host prototypes are not supported in snapshots",
            ));
        }
        let module_id = image.module_id.map(crate::ModuleId::from);
        let mut proto = Self {
            native: image.native,
            host: image.host,
            max_stack_size: image.max_stack_size,
            num_params: image.num_params,
            num_upvalues: image.num_upvalues,
            is_vararg: image.is_vararg,
            code: image.code,
            jump_targets: image.jump_targets,
            constants: image.constants,
            import_values: image.import_values,
            child_protos: image.child_protos,
            line_defined: image.line_defined,
            debug_name: image.debug_name,
            lines: image.lines,
            coverage_hits: image.coverage_hits,
            source: image.source,
            module_id,
            charged_footprint: 0,
            populated: image.populated,
        };
        proto.charged_footprint = proto.footprint_from_buffers();
        Ok(proto)
    }

    fn footprint_from_buffers(&self) -> usize {
        self.code.capacity() * size_of::<Instruction>()
            + self.jump_targets.capacity() * size_of::<u32>()
            + self.constants.capacity() * size_of::<RuntimeConstant>()
            + self.child_protos.capacity() * size_of::<RawGc<Self>>()
            + self.lines.capacity() * size_of::<u32>()
            + self.coverage_hits.capacity() * size_of::<u32>()
    }
}

/// The loader-resolved buffers for a bytecode prototype.
pub struct ProtoBuffers {
    pub code: Vec<Instruction>,
    pub jump_targets: Vec<u32>,
    pub constants: Vec<RuntimeConstant>,
    pub child_protos: Vec<RawGc<Proto>>,
    pub lines: Vec<u32>,
    pub coverage_hits: Vec<u32>,
    pub source: RawGc<marker::Str>,
    pub module_id: Option<crate::ModuleId>,
    pub debug_name: Option<RawGc<marker::Str>>,
}

impl ProtoBuffers {
    #[must_use]
    pub(crate) fn footprint(&self) -> usize {
        self.code.capacity() * size_of::<Instruction>()
            + self.jump_targets.capacity() * size_of::<u32>()
            + self.constants.capacity() * size_of::<RuntimeConstant>()
            + self.child_protos.capacity() * size_of::<RawGc<Proto>>()
            + self.lines.capacity() * size_of::<u32>()
            + self.coverage_hits.capacity() * size_of::<u32>()
    }
}

/// One entry in a loaded proto's constant table.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub enum RuntimeConstant {
    /// A fully resolved value: `nil`, boolean, number, integer, string, or
    /// vector.
    Value(RawValue),
    /// A deferred import id, resolved against the environment when an
    /// import-bearing opcode runs.
    Import(u32),
    /// A table-shape template for `NEWTABLE` / `DUPTABLE`.
    Table(TableShape),
    /// A child-proto reference for `NEWCLOSURE` / `DUPCLOSURE`.
    Proto(RawGc<Proto>),
}

/// A table-shape template materialized from a `Table` or `TableWithConstants`
/// constant. The interpreter clones it to build a table at runtime.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub struct TableShape {
    /// Preset key/value entries (from `TableWithConstants`); empty for a bare
    /// shape that only hints capacity.
    pub entries: Vec<(RawValue, RawValue)>,
    /// Hint for how many array slots to preallocate.
    pub array_hint: u32,
}

/// Host userdata object: an embedder-typed value owned by the heap.
///
/// The payload lives in the VM's segmented [`HostPayloadStore`]
/// (crate::host_type::HostPayloadStore), created through `Scope::create_userdata`
/// for a host type registered at VM build. This GC object carries the stable
/// store identity, host-type index, and memory charge.
///
/// Memory accounting follows the buffer model, with the release on `Drop`
/// instead of a sweep hook: the boxed payload's size is charged to the heap
/// meter when the userdata is allocated ([`attach_meter`](Self::attach_meter))
/// and released when the object is dropped — by a GC sweep reclaiming the slot,
/// or by the heap (and every arena) dropping with the `Vm`.
pub struct LuaUserdata {
    payload_id: crate::host_type::PayloadId,
    /// Index of the value's registered host type in the heap's host-type
    /// registry (metatable, name, declaration).
    type_index: u32,
    /// The boxed payload's byte footprint, charged on attach.
    payload_size: usize,
    meter: MemoryMeter,
    /// Bytes currently charged on `meter`; released exactly once, in `Drop`.
    charged: usize,
}

impl LuaUserdata {
    /// Records an already-stored host payload, charging nothing until the
    /// userdata is allocated into a heap. `payload_size` is the byte footprint
    /// to charge for the boxed embedder value `T`.
    #[must_use]
    pub(crate) fn new(
        payload_id: crate::host_type::PayloadId,
        type_index: u32,
        payload_size: usize,
    ) -> Self {
        Self {
            payload_id,
            type_index,
            payload_size,
            meter: MemoryMeter::default(),
            charged: 0,
        }
    }

    /// Points the userdata at the heap's shared meter and charges its payload
    /// footprint — called when the userdata is allocated into the heap. A
    /// clean hand-off like the buffer/stack pattern: any previous charge is
    /// released first, so a re-attach never double-counts.
    pub(crate) fn attach_meter(&mut self, meter: MemoryMeter) {
        self.meter.adjust(self.charged, 0);
        self.meter = meter;
        self.charged = self.payload_size;
        self.meter.adjust(0, self.charged);
    }

    /// The address-stable payload-store identity.
    #[must_use]
    pub(crate) fn payload_id(&self) -> crate::host_type::PayloadId {
        self.payload_id
    }

    /// The host-type registry index this value was created under.
    #[must_use]
    pub(crate) fn type_index(&self) -> u32 {
        self.type_index
    }

    /// The metered byte footprint charged for the boxed payload. Reported to
    /// `gcinfo()`; the release itself happens in `Drop`.
    pub(crate) fn gc_footprint(&self) -> usize {
        self.charged
    }
}

impl Drop for LuaUserdata {
    fn drop(&mut self) {
        // Release the payload charge on the owning meter. Unlike buffers (whose
        // release is a sweep hook), userdata releases here so every drop path —
        // GC sweep, arena compaction, allocation failure, VM teardown — settles
        // the meter exactly once.
        self.meter.adjust(self.charged, 0);
        self.charged = 0;
    }
}

/// A fixed-size byte buffer (`buffer.create`), the backing store for the
/// `buffer` library's little-endian reads and writes. Its size is set at
/// creation and never changes; the bytes count against the heap's memory cap.
pub struct LuaBuffer {
    data: Vec<u8>,
    meter: MemoryMeter,
    charged: usize,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct LuaBufferImage {
    pub data: Vec<u8>,
}

impl LuaBuffer {
    /// A zero-filled buffer of `size` bytes, reserving fallibly before allocation.
    ///
    /// # Errors
    /// Returns [`std::collections::TryReserveError`] when reserving the backing
    /// storage fails.
    pub(crate) fn try_with_size(size: usize) -> Result<Self, std::collections::TryReserveError> {
        let mut data = Vec::new();
        data.try_reserve_exact(size)?;
        data.resize(size, 0);
        Ok(Self {
            data,
            meter: MemoryMeter::default(),
            charged: 0,
        })
    }

    /// A buffer initialized from `bytes` (`buffer.fromstring`).
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            data: bytes.to_vec(),
            meter: MemoryMeter::default(),
            charged: 0,
        }
    }

    /// A buffer initialized from `bytes`, reserving fallibly before allocation.
    ///
    /// # Errors
    /// Returns [`std::collections::TryReserveError`] when reserving the backing
    /// storage fails.
    pub(crate) fn try_from_bytes(bytes: &[u8]) -> Result<Self, std::collections::TryReserveError> {
        let mut data = Vec::new();
        data.try_reserve_exact(bytes.len())?;
        data.extend_from_slice(bytes);
        Ok(Self {
            data,
            meter: MemoryMeter::default(),
            charged: 0,
        })
    }

    /// Points the buffer at the heap's shared meter and charges its footprint —
    /// called when the buffer is allocated into the heap.
    pub fn attach_meter(&mut self, meter: MemoryMeter) {
        self.meter.adjust(self.charged, 0);
        self.meter = meter;
        self.charged = self.data.capacity();
        self.meter.adjust(0, self.charged);
    }

    /// The buffer length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// GC: the metered byte footprint to release when this buffer is swept (its
    /// charged backing capacity).
    pub(crate) fn gc_footprint(&self) -> usize {
        self.charged
    }

    /// The bytes, for reads.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// The bytes, mutably, for writes.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub(crate) fn snapshot_image(&self) -> LuaBufferImage {
        LuaBufferImage {
            data: self.data.clone(),
        }
    }

    pub(crate) fn from_snapshot_image(image: LuaBufferImage) -> Self {
        Self {
            data: image.data,
            meter: MemoryMeter::default(),
            charged: 0,
        }
    }
}
