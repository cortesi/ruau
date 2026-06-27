//! The accounted arena heap.
//!
//! Every collectible object lives in a typed arena owned by the VM; a handle is
//! a generational index, not a pointer. The mark-sweep collector
//! ([`mod@crate::gc`]) drives sweep through [`Arena::gc_sweep`].

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ptr::NonNull,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use ruau_vm_api::{HeapId, HostFunction, RawGc, RawValue, RegistryRef, marker};

use crate::{
    PrintSink,
    builtins::Builtin,
    func::{Closure, UpVal},
    gas_profile::{GasProfile, GasProfileRecorder, GasProfileSite},
    gc::GcRef,
    hash::VmBuildHasher,
    host::{AsyncHostFunction, HostCallable, ScopedHostFunction},
    limits::{AmbientConfig, AmbientMode, EffectiveLimits, GcPolicy},
    object::{HostId, LuaBuffer, LuaUserdata, Proto, ProtoBuffers},
    runtime_compile::{
        RuntimeCompileContext, RuntimeCompileLimits, RuntimeCompiler, VmRuntimeCompiler,
    },
    scope::{AppData, ContextSlot},
    snapshot::SnapshotError,
    state::{CoroutineStatus, Thread},
    string::{InternedString, StringInterner},
    table::LuaTable,
};

mod arena;
mod host_context;
mod module_cache;
mod random;
mod registry;
mod store;

pub use arena::{AccountedVec, Age, Arena, ArenaEntry, Color, MemoryMeter};
use host_context::{HostAppDataGuard, HostAppDataPtr, HostContextGuard, HostContextPtr};
use module_cache::{
    ArenaEntryImage, ArenaImage, InstanceKeyImage, ModuleCacheEntry, ObjectStoreImage,
    RegistryImage, RegistryRefImage, RegistrySlotImage, ThreadImage, rebrand_heap_image,
    restore_empty_userdata_arena, snapshot_closure, snapshot_upval,
};
pub use module_cache::{HeapImage, ModuleCacheKey};
use random::{GC_RNG_SEED_SALT, GC_STRESS_STRIDE, pcg32_output, pcg32_seed, pcg32_step};
use registry::Registry;
use store::ObjectStore;
pub use store::StackStore;

/// The accounted arena heap of one VM.
pub struct Heap {
    /// Per-VM nonce stamped into every handle.
    pub id: HeapId,
    pub(crate) objects: ObjectStore,
    registry: Registry,
    /// String-keyed session state: each entry roots its value through a registry
    /// pin (so no extra GC tracing is needed), keyed by a host-chosen name. The
    /// host populates it through the borrowed `Scope`; an untrusted script cannot
    /// reach it. **Host-owned and out of the VM byte meter** — the pin is charged,
    /// but the map and key bytes are not; this is safe because only the host (never
    /// a tenant) feeds keys. A per-run namespace with key/count caps lands with the
    /// execution-session work for any tenant-derived name volume.
    named: HashMap<Vec<u8>, RegistryRef>,
    /// Deferred `Stashed` releases. A `Stashed` dropped off-lane cannot unpin
    /// synchronously (it has no heap access), so its last clone sends its pin here;
    /// the lane drains and unpins these at the start of each step. The send never
    /// blocks or panics — if the VM (and this receiver) is gone, the pin leaks by
    /// contract.
    release_tx: std::sync::mpsc::Sender<RegistryRef>,
    release_rx: std::sync::mpsc::Receiver<RegistryRef>,
    pub(crate) interner: StringInterner,
    hash_builder: VmBuildHasher,
    /// The per-VM byte counter every growable container charges (`==`
    /// `global_State::totalbytes`); `memory_cap` is the ceiling the dispatch
    /// safepoint checks it against.
    meter: MemoryMeter,
    /// The configured in-VM memory ceiling (`Limits::max_memory_bytes`); `None`
    /// is unbounded (the process backstop still applies).
    memory_cap: Option<usize>,
    /// Concrete per-invocation ceilings for builtin and call-side operations
    /// whose allocations/results need an absolute bound even without a memory cap.
    limits: EffectiveLimits,
    /// Compiler used by runtime source compilation (`loadstring`).
    runtime_compiler: Arc<dyn RuntimeCompiler>,
    /// Whether the VM's profile enables runtime source compilation. Gates the
    /// host-initiated `Scope::load_chunk`/`Scope::eval_chunk` entry points at
    /// call time; the script-facing `loadstring` global is gated at install
    /// instead. `false` until the builder applies the profile, so a hand-built
    /// heap fails closed.
    runtime_compilation_enabled: bool,
    /// Source provider for `require`, when configured. `None` leaves `require`
    /// uninstalled (an embedder opts in by supplying one).
    module_source: Option<Arc<dyn crate::ModuleSource>>,
    /// Whether build-time native modules make `require` meaningful even without
    /// a runtime source provider. Set before globals are installed so the base
    /// surface includes the builtin when native require exports will be rooted
    /// later in VM construction.
    native_require_enabled: bool,
    /// Build-time native module exports, keyed by canonical native module id.
    /// These are host surface roots, so `clear_module_cache` leaves them intact.
    native_module_exports: HashMap<crate::ModuleId, RegistryRef>,
    /// `require`'s module cache: a required module is run once and its exports are
    /// pinned here under the source-provided instance key. The entry records the
    /// source epoch, so a source update invalidates stale exports without growing
    /// cache keys forever.
    module_cache: HashMap<crate::InstanceKey, ModuleCacheEntry>,
    /// Instance keys plus source epochs whose module bodies are currently running.
    module_loading: HashSet<ModuleCacheKey>,
    /// Monotonic id for async VM calls. Used to distinguish coroutines touched
    /// by the current fatal request from retained-session coroutines parked by
    /// earlier successful calls.
    next_async_invocation: u64,
    active_async_invocation: Option<u64>,
    /// A host sink for `print`/log output. `print` formats its arguments and writes
    /// the line here; `None` discards (the default — `print` is a no-op). The host
    /// owns and bounds the destination, so an untrusted script's print volume is the
    /// host's to cap.
    print_sink: Option<PrintSink>,
    /// The request's cancellation token, polled at the dispatch safepoint so a
    /// synchronous CPU loop can be cancelled, not only a parked async await.
    cancel: Option<crate::cancel::Cancel>,
    gc_policy: GcPolicy,
    /// The remaining instruction budget for this request, shared by every thread
    /// `None` is unlimited. The dispatch loop spends one unit per instruction
    /// and a depleted budget halts execution, so untrusted bytecode cannot spin
    /// a loop unmetered. The cooperative `Step::Preempt` quantum provides the
    /// fairness slice.
    gas: Option<u64>,
    /// Logical (gas-tick) deadline for the current invocation, enforced at
    /// the dispatch safepoint against `gas_spent`.
    logical_deadline: Option<u64>,
    /// Gas units spent by the current or most recent invocation.
    gas_spent: u64,
    /// Active per-invocation gas profiler, installed only while profiling is
    /// explicitly enabled for the current call.
    active_gas_profile: Option<GasProfileRecorder>,
    /// The most recently completed profiled invocation. Cleared at the start of
    /// every invocation, profiled or not, so callers never read stale data.
    gas_profile: Option<GasProfile>,
    /// The cooperative scheduling quantum (instructions per slice) and the count
    /// remaining in the current slice; `None` disables preemption.
    quantum: Option<u64>,
    quantum_remaining: u64,
    /// The shared metatable for every string value (`l_registry`'s string
    /// metatable, `lstate.h`), so `("s"):method()` resolves through the `string`
    /// library. Bound at VM build.
    string_metatable: Option<RawGc<marker::Table>>,
    /// The shared metatable for the `vector` type (upstream `global->mt[LUA_TVECTOR]`),
    /// installed by the host so a vector resolves `__index`/`__namecall`. Component
    /// access (`.x`/`.y`/`.z`) is a VM fast path that does not consult it.
    vector_metatable: Option<RawGc<marker::Table>>,
    /// Registered host functions, indexed by `HostId`. Slots are shared
    /// (`Arc`), so a dispatch clones the handle instead of emptying the slot —
    /// the call can hold `&mut Heap` without aliasing the registry, and a host
    /// function that re-enters the VM can recursively dispatch itself.
    host_functions: Vec<Arc<HostCallable>>,
    /// Typed host-error payloads parked across a script `pcall` catch, keyed by
    /// error-value identity. Touched only on error materialization and host
    /// exit-surface recovery; see [`crate::call::HostPayloadTracker`].
    pub(crate) host_error_payloads: crate::call::HostPayloadTracker,
    /// Registered host userdata types, indexed by `LuaUserdata::type_index`.
    /// Built once at VM build ([`crate::host_type::install_host_types`]); each
    /// entry roots its shared metatable and method table through registry pins,
    /// so userdata objects stay GC leaves.
    host_types: Vec<crate::host_type::HostTypeRuntime>,
    /// VM app data visible to scoped host calls while a VM entry is active.
    /// Boxed into its own allocation so [`HostAppDataGuard`]'s restore write
    /// stays valid while the dispatch body re-borrows `&mut Heap`: a pointer
    /// into the `Heap` allocation itself would be invalidated by those
    /// re-borrows (Stacked Borrows), but the boxed cell is a distinct
    /// allocation that never moves while a guard is live.
    host_app_data: Box<Cell<Option<HostAppDataPtr>>>,
    /// Borrowed host context visible to scoped host calls while a VM entry is active.
    host_context: Box<Cell<Option<HostContextPtr>>>,
    /// Pre-interned metamethod event names, indexed by `MetaEvent`
    /// discriminant. Populated at construction and marked as GC roots (the
    /// interner is weak), so a metamethod probe never re-hashes its name.
    pub(crate) metamethod_names: [Option<RawGc<marker::Str>>; 18],
    /// The `math.random` generator state (upstream `global_State::rngstate`). One
    /// PCG32 stream per VM, seeded at build from `AmbientConfig::prng_seed` so a
    /// deterministic VM replays an identical sequence; `math.randomseed` reseeds it.
    rngstate: u64,
    /// The `GcPolicy::RandomizedSteps` PRNG state — a second PCG32 stream seeded from the
    /// same config seed mixed with [`GC_RNG_SEED_SALT`], so the seeded GC schedule is
    /// reproducible yet independent of (and not perturbable through) `math.random`.
    gc_rng: u64,
    /// The clock seam for the `os` library. In production it reads the
    /// real wall/monotonic clock; under the deterministic seam it returns frozen
    /// values so an adversarial snippet observes no wall-clock side channel.
    ambient_mode: AmbientMode,
    /// The monotonic baseline for `os.clock`, captured at build in production mode
    /// and `None` under the deterministic seam.
    clock_start: Option<Instant>,
    /// Set by `collectgarbage("collect")` and honoured at the next root dispatch
    /// safepoint, which runs `collect_active` — so a request from inside a coroutine
    /// cannot trigger an unsound collection (the single-take-out contract holds only
    /// on the root paths); it just defers to the next safepoint that path reaches.
    gc_requested: bool,
    /// The allocation debt threshold (`==` Luau's `global_State::GCthreshold`): once
    /// the metered footprint reaches it, a routine `GcPolicy::Threshold` collection is
    /// due. Reset to a multiple of the live footprint after each collection (see
    /// [`Self::note_collection`]), so collection paces with allocation rather than only
    /// firing at the hard memory cap.
    gc_threshold: usize,
    /// Completed collection cycles over this heap's lifetime — observability for pacing
    /// tests and host telemetry.
    gc_cycles: u64,
    /// Whether routine allocation-debt collection is running. Manual collection requests
    /// still run while stopped; this only mirrors `collectgarbage("stop")` for automatic
    /// threshold pacing.
    gc_running: bool,
    /// Coarse progress for manual `collectgarbage("step", size)`. Ruau's collector is
    /// stop-the-world rather than incremental, so this emulates Luau's completion signal by
    /// accumulating requested work and scheduling one ordinary top-level collection once a
    /// small cycle budget is reached.
    gc_step_progress: usize,
    /// A manual GC step has already scheduled a collection and should report completion on
    /// the next `collectgarbage("step", ...)` call. This gives the dispatch safepoint a
    /// chance to make the requested collection visible before Lua observes the completed
    /// step.
    gc_step_ready: bool,
    /// Number of arena-resident threads currently taken out into Rust locals for
    /// execution. Active GC is sound only at depth 1.
    taken_out_threads: u32,
    /// Whether a borrowed [`Scope`](crate::Scope) step is active on this lane. The
    /// re-entry guard: opening a nested scope (a host re-entering the VM mid-step)
    /// while one is already active is rejected, so the single-active-borrow
    /// property holds even once host calls can re-enter.
    scope_active: bool,
    /// Generational remembered set: `Old` objects the write barrier saw store a heap
    /// reference, so a minor collection traces them as roots (they may point at a young
    /// object the minor would otherwise miss). Each entry's slot is flipped to
    /// `Age::OldRemembered` when added, so an object is remembered at most once per minor;
    /// a minor clears this and reverts the ages to `Old`.
    pub(crate) gc_remembered: Vec<GcRef>,
    /// Minor collections since the last major — a safety cap so old garbage (and an
    /// unreachable suspended coroutine, which a minor keeps alive as a root) is reclaimed
    /// within a bounded number of minors even if allocation never crosses the major
    /// threshold below. See [`GC_MAJOR_SAFETY_CAP`].
    pub(crate) gc_minors_since_major: u32,
    /// The metered footprint at (or above) which the next collection is promoted to a major,
    /// reclaiming old garbage and compacting. Set after each major to the post-major
    /// footprint plus [`GC_MAJOR_STEP_BYTES`], so majors pace with *allocation* (footprint
    /// growth) rather than collection count — important under `CollectOnAllocation`, where a
    /// per-collection-count schedule would run a full major every few allocations. A
    /// non-growing heap (transient churn under minors) never crosses it, so its minors stay
    /// bounded by the young set.
    pub(crate) gc_major_threshold: usize,
    /// Set when the write barrier could not grow the remembered set under memory pressure.
    /// The next collection is then forced to be a major (which needs no remembered set), so
    /// an unrecordable old→young edge can never make a minor miss a live young object.
    pub(crate) gc_force_major: bool,
    /// Test-only: when set, a minor collection returns `GcAbort` after processing its roots,
    /// simulating a work-list allocation failure. Used to regression-test the abort-recovery
    /// path (the remembered set must survive an aborted minor; see `collect_minor_inner`).
    #[cfg(any())]
    pub(crate) gc_test_abort_minor: bool,
}

/// Footprint growth since the last major that promotes the next collection to a major (so
/// majors pace with allocation, not collection count).
const GC_MAJOR_STEP_BYTES: usize = 4 * 1024 * 1024;
/// A major runs after at most this many minors regardless of allocation, so old garbage and
/// unreachable coroutines a minor keeps alive cannot accumulate unboundedly when allocation
/// is slow. Large enough that it does not dominate under `CollectOnAllocation` (where it
/// would otherwise run a major every few allocations).
const GC_MAJOR_SAFETY_CAP: u32 = 256;

/// GC pacing (`GcPolicy::Threshold`): the debt threshold is advanced by this many
/// bytes of footprint growth after each collection, so routine GC runs once per
/// step of allocation rather than only at the hard cap. The step is additive (a
/// fixed allocation budget) rather than a multiple of the live set because, until
/// a swept slot's arena capacity is released (the tracked capacity-release work),
/// the footprint retains reclaimed slot capacity — a multiplicative pause computed
/// from that inflated footprint would ratchet upward without bound. Once capacity
/// tracks the live set this becomes a live-set-relative pause (Luau's `gcpause`).
const GC_STEP_BYTES: usize = 256 * 1024;
const GC_MANUAL_STEP_UNITS: usize = 12;

impl Heap {
    /// Builds an empty heap for a VM with the given nonce and baked config.
    #[must_use]
    pub fn new(id: HeapId, config: AmbientConfig) -> Self {
        let meter = MemoryMeter::default();
        let hash_builder = VmBuildHasher::new(config.hash_seed);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut heap = Self {
            id,
            objects: ObjectStore::with_meter(&meter),
            registry: Registry::with_meter(meter.clone()),
            named: HashMap::new(),
            host_error_payloads: crate::call::HostPayloadTracker::default(),
            module_source: None,
            native_require_enabled: false,
            native_module_exports: HashMap::new(),
            module_cache: HashMap::new(),
            module_loading: HashSet::new(),
            next_async_invocation: 0,
            active_async_invocation: None,
            print_sink: None,
            release_tx,
            release_rx,
            interner: StringInterner::with_meter_and_hash(meter.clone(), hash_builder),
            hash_builder,
            meter,
            memory_cap: None,
            limits: EffectiveLimits::default(),
            runtime_compiler: Arc::new(VmRuntimeCompiler::default()),
            runtime_compilation_enabled: false,
            cancel: None,
            gc_policy: config.gc_policy,
            gas: None,
            logical_deadline: None,
            metamethod_names: [None; 18],
            gas_spent: 0,
            active_gas_profile: None,
            gas_profile: None,
            quantum: None,
            quantum_remaining: 0,
            string_metatable: None,
            vector_metatable: None,
            host_functions: Vec::new(),
            host_types: Vec::new(),
            host_app_data: Box::new(Cell::new(None)),
            host_context: Box::new(Cell::new(None)),
            rngstate: pcg32_seed(config.prng_seed),
            gc_rng: pcg32_seed(config.prng_seed ^ GC_RNG_SEED_SALT),
            ambient_mode: AmbientMode::Deterministic(0),
            clock_start: None,
            gc_requested: false,
            gc_threshold: GC_STEP_BYTES,
            gc_cycles: 0,
            gc_running: true,
            gc_step_progress: 0,
            gc_step_ready: false,
            taken_out_threads: 0,
            scope_active: false,
            gc_remembered: Vec::new(),
            gc_minors_since_major: 0,
            gc_major_threshold: GC_MAJOR_STEP_BYTES,
            gc_force_major: false,
            #[cfg(any())]
            gc_test_abort_minor: false,
        };
        // Intern the metamethod event names once; `get_metamethod` reads the
        // cached, GC-rooted handles instead of re-hashing per probe. Tiny
        // fixed strings: the allocation cannot realistically fail here, and a
        // `None` slot degrades to the interning-failed error at probe time.
        for event in crate::tm::MetaEvent::ALL {
            heap.metamethod_names[event as usize] = heap.intern_str(event.name());
        }
        heap
    }

    pub(crate) fn host_function_count(&self) -> usize {
        self.host_functions.len()
    }

    pub(crate) fn host_type_count(&self) -> usize {
        self.host_types.len()
    }

    pub(crate) fn module_source_present(&self) -> bool {
        self.module_source.is_some()
    }

    pub(crate) fn snapshot_image(&mut self) -> Result<HeapImage, SnapshotError> {
        self.drain_releases();
        self.check_snapshot_ready()?;
        let objects = ObjectStoreImage {
            strings: self
                .objects
                .strings
                .snapshot_image_with(|string| Ok(string.bytes().to_vec()))?,
            tables: self
                .objects
                .tables
                .snapshot_image_with(|table| Ok(table.snapshot_image()))?,
            closures: self
                .objects
                .closures
                .snapshot_image_with(|closure| Ok(snapshot_closure(closure)))?,
            userdata: self.objects.userdata.snapshot_image_with(|_| {
                Err(SnapshotError::Unsupported(
                    "live host userdata is not in the prototype codec",
                ))
            })?,
            threads: self
                .objects
                .threads
                .snapshot_image_with(ThreadImage::from_thread)?,
            buffers: self
                .objects
                .buffers
                .snapshot_image_with(|buffer| Ok(buffer.snapshot_image()))?,
            protos: self
                .objects
                .protos
                .snapshot_image_with(|proto| Ok(proto.snapshot_image()))?,
            upvals: self
                .objects
                .upvals
                .snapshot_image_with(|upval| Ok(snapshot_upval(upval)))?,
        };

        Ok(HeapImage {
            objects,
            registry: self.registry.snapshot_image(),
            named: self
                .named
                .iter()
                .map(|(name, reference)| (name.clone(), RegistryRefImage::from_ref(reference)))
                .collect(),
            module_cache: self
                .module_cache
                .iter()
                .map(|(instance, entry)| {
                    (
                        InstanceKeyImage::from(instance),
                        entry.epoch,
                        RegistryRefImage::from_ref(&entry.reference),
                    )
                })
                .collect(),
            next_async_invocation: self.next_async_invocation,
            gas: self.gas,
            logical_deadline: self.logical_deadline,
            gas_spent: self.gas_spent,
            quantum: self.quantum,
            quantum_remaining: self.quantum_remaining,
            string_metatable: self.string_metatable,
            vector_metatable: self.vector_metatable,
            metamethod_names: self.metamethod_names,
            rngstate: self.rngstate,
            gc_rng: self.gc_rng,
            ambient_mode: self.ambient_mode,
            gc_requested: self.gc_requested,
            gc_threshold: self.gc_threshold,
            gc_cycles: self.gc_cycles,
            gc_running: self.gc_running,
            gc_step_progress: self.gc_step_progress,
            gc_step_ready: self.gc_step_ready,
            gc_remembered: self.gc_remembered.clone(),
            gc_minors_since_major: self.gc_minors_since_major,
            gc_major_threshold: self.gc_major_threshold,
            gc_force_major: self.gc_force_major,
        })
    }

    fn check_snapshot_ready(&self) -> Result<(), SnapshotError> {
        if !matches!(self.ambient_mode, AmbientMode::Deterministic(_)) {
            return Err(SnapshotError::Unsupported(
                "production ambient state is not deterministic",
            ));
        }
        if self.taken_out_threads != 0 {
            return Err(SnapshotError::NotQuiescent("thread is currently running"));
        }
        if self.scope_active {
            return Err(SnapshotError::NotQuiescent("scope step is active"));
        }
        if self.host_app_data.get().is_some() {
            return Err(SnapshotError::NotQuiescent("host app data is installed"));
        }
        if self.active_async_invocation.is_some() {
            return Err(SnapshotError::NotQuiescent("async invocation is active"));
        }
        if !self.module_loading.is_empty() {
            return Err(SnapshotError::NotQuiescent("module body is loading"));
        }
        if self.module_source.is_some() {
            return Err(SnapshotError::Unsupported(
                "runtime module sources are not in the prototype codec",
            ));
        }
        if !self.native_module_exports.is_empty() {
            return Err(SnapshotError::Unsupported(
                "native module exports are not in the prototype codec",
            ));
        }
        if !self.host_functions.is_empty() {
            return Err(SnapshotError::Unsupported(
                "registered host functions are not in the prototype codec",
            ));
        }
        if !self.host_types.is_empty() {
            return Err(SnapshotError::Unsupported(
                "registered host userdata types are not in the prototype codec",
            ));
        }
        Ok(())
    }

    pub(crate) fn from_snapshot_image(
        template: Self,
        mut image: HeapImage,
    ) -> Result<Self, SnapshotError> {
        let id = template.id;
        rebrand_heap_image(&mut image, id);
        image.normalize_gc_metadata()?;
        let meter = MemoryMeter::default();
        let hash_builder = template.hash_builder;
        let strings = image
            .objects
            .strings
            .restore_arena(meter.clone(), InternedString::new)?;
        let tables = image.objects.tables.restore_arena(meter.clone(), |table| {
            LuaTable::from_snapshot_image(table, hash_builder, meter.clone())
        })?;
        let closures = image
            .objects
            .closures
            .restore_arena(meter.clone(), |closure| closure)?;
        let userdata = restore_empty_userdata_arena(image.objects.userdata, meter.clone())?;
        let threads = image
            .objects
            .threads
            .restore_arena(meter.clone(), |thread| thread.into_thread(meter.clone()))?;
        let buffers = image
            .objects
            .buffers
            .restore_arena(meter.clone(), |buffer| {
                let mut buffer = LuaBuffer::from_snapshot_image(buffer);
                buffer.attach_meter(meter.clone());
                buffer
            })?;
        let protos = image
            .objects
            .protos
            .try_restore_arena(meter.clone(), |proto| {
                let proto = Proto::from_snapshot_image(proto)?;
                meter.charge(proto.footprint());
                Ok(proto)
            })?;
        let upvals = image
            .objects
            .upvals
            .restore_arena(meter.clone(), |upval| upval)?;
        let objects = ObjectStore {
            strings,
            tables,
            closures,
            userdata,
            threads,
            buffers,
            protos,
            upvals,
        };
        let mut interner = StringInterner::with_meter_and_hash(meter.clone(), hash_builder);
        for index in 0..objects.strings.len() as u32 {
            let Some(string) = objects.strings.gc_value(index) else {
                continue;
            };
            let generation = *objects
                .strings
                .gens
                .get(index as usize)
                .ok_or(SnapshotError::Invalid("string generation missing"))?;
            let handle = RawGc::from_parts(index, generation, id);
            meter.charge(string.len());
            interner.insert(string.bytes(), handle);
        }

        let registry = Registry::from_snapshot_image(image.registry, meter.clone(), id);
        let named = image
            .named
            .into_iter()
            .map(|(name, reference)| (name, reference.into_ref(id)))
            .collect();
        let module_cache = image
            .module_cache
            .into_iter()
            .map(|(instance, epoch, reference)| {
                (
                    crate::InstanceKey::from(instance),
                    ModuleCacheEntry {
                        epoch,
                        reference: reference.into_ref(id),
                    },
                )
            })
            .collect();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        Ok(Self {
            id,
            objects,
            registry,
            named,
            release_tx,
            release_rx,
            interner,
            hash_builder,
            meter,
            memory_cap: template.memory_cap,
            limits: template.limits,
            runtime_compiler: template.runtime_compiler,
            runtime_compilation_enabled: template.runtime_compilation_enabled,
            module_source: None,
            native_require_enabled: false,
            native_module_exports: HashMap::new(),
            module_cache,
            module_loading: HashSet::new(),
            next_async_invocation: image.next_async_invocation,
            active_async_invocation: None,
            print_sink: template.print_sink,
            cancel: None,
            gc_policy: template.gc_policy,
            gas: image.gas,
            logical_deadline: image.logical_deadline,
            gas_spent: image.gas_spent,
            active_gas_profile: None,
            gas_profile: None,
            quantum: image.quantum,
            quantum_remaining: image.quantum_remaining,
            string_metatable: image.string_metatable,
            vector_metatable: image.vector_metatable,
            host_functions: template.host_functions,
            host_error_payloads: crate::call::HostPayloadTracker::default(),
            host_types: template.host_types,
            host_app_data: Box::new(Cell::new(None)),
            host_context: Box::new(Cell::new(None)),
            metamethod_names: image.metamethod_names,
            rngstate: image.rngstate,
            gc_rng: image.gc_rng,
            ambient_mode: image.ambient_mode,
            clock_start: None,
            gc_requested: image.gc_requested,
            gc_threshold: image.gc_threshold,
            gc_cycles: image.gc_cycles,
            gc_running: image.gc_running,
            gc_step_progress: image.gc_step_progress,
            gc_step_ready: image.gc_step_ready,
            taken_out_threads: 0,
            scope_active: false,
            gc_remembered: image.gc_remembered,
            gc_minors_since_major: image.gc_minors_since_major,
            gc_major_threshold: image.gc_major_threshold,
            gc_force_major: image.gc_force_major,
            #[cfg(any())]
            gc_test_abort_minor: false,
        })
    }

    /// Selects the clock seam (`os` library). Called at build with the VM's
    /// ambient mode; production captures the monotonic baseline for `os.clock`.
    pub fn set_clock(&mut self, mode: AmbientMode) {
        self.ambient_mode = mode;
        self.clock_start = match mode {
            AmbientMode::Production => Some(Instant::now()),
            AmbientMode::Deterministic(_) => None,
        };
    }

    /// Wall-clock seconds since the Unix epoch for `os.time()`/`os.date()`: the
    /// real time in production, a caller-selected frozen timestamp under the
    /// deterministic seam.
    #[must_use]
    pub fn wall_time_secs(&self) -> f64 {
        match self.ambient_mode {
            AmbientMode::Production => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs() as f64),
            AmbientMode::Deterministic(secs) => secs as f64,
        }
    }

    /// Monotonic seconds since VM build for `os.clock`: real elapsed time in
    /// production, a frozen `0.0` under the deterministic seam.
    #[must_use]
    pub fn process_clock_secs(&self) -> f64 {
        self.clock_start
            .map_or(0.0, |start| start.elapsed().as_secs_f64())
    }

    /// Draws the next 32 bits from the VM's `math.random` stream (upstream
    /// `pcg32_random`) and advances the state.
    pub fn next_random_u32(&mut self) -> u32 {
        let oldstate = self.rngstate;
        self.rngstate = pcg32_step(oldstate);
        pcg32_output(oldstate)
    }

    /// Reseeds the `math.random` stream (upstream `math_randomseed`).
    pub fn seed_random(&mut self, seed: u64) {
        self.rngstate = pcg32_seed(seed);
    }

    /// Sets the in-VM memory ceiling (`Limits::max_memory_bytes`), checked at the
    /// dispatch safepoint. `None` is unbounded.
    pub fn set_memory_cap(&mut self, cap: Option<usize>) {
        self.memory_cap = cap;
    }

    /// Sets the concrete per-invocation operation ceilings.
    pub(crate) fn set_limits(&mut self, limits: EffectiveLimits) {
        self.limits = limits;
    }

    /// Returns the concrete per-invocation operation ceilings.
    #[must_use]
    pub(crate) fn limits(&self) -> EffectiveLimits {
        self.limits
    }

    pub(crate) fn set_runtime_compiler(&mut self, compiler: Arc<dyn RuntimeCompiler>) {
        self.runtime_compiler = compiler;
    }

    #[must_use]
    pub(crate) fn runtime_compiler(&self) -> Arc<dyn RuntimeCompiler> {
        Arc::clone(&self.runtime_compiler)
    }

    /// Records whether the VM's profile enables runtime source compilation —
    /// the call-time gate for `Scope::load_chunk`/`Scope::eval_chunk`.
    pub(crate) fn set_runtime_compilation_enabled(&mut self, enabled: bool) {
        self.runtime_compilation_enabled = enabled;
    }

    /// Whether the VM's profile enables runtime source compilation.
    #[must_use]
    pub(crate) fn runtime_compilation_enabled(&self) -> bool {
        self.runtime_compilation_enabled
    }

    pub(crate) fn set_module_source(&mut self, source: Arc<dyn crate::ModuleSource>) {
        self.module_source = Some(source);
    }

    pub(crate) fn enable_native_require(&mut self) {
        self.native_require_enabled = true;
    }

    #[must_use]
    pub(crate) fn require_available(&self) -> bool {
        self.module_source.is_some() || self.native_require_enabled
    }

    #[must_use]
    pub(crate) fn module_source(&self) -> Option<Arc<dyn crate::ModuleSource>> {
        self.module_source.clone()
    }

    #[must_use]
    pub(crate) fn native_module_export_get(&self, id: &crate::ModuleId) -> Option<RawValue> {
        let reference = self.native_module_exports.get(id)?;
        self.pinned_value(reference).ok()
    }

    pub(crate) fn native_module_export_set(
        &mut self,
        id: crate::ModuleId,
        exports: RawValue,
    ) -> Option<()> {
        let reference = self.pin(exports)?;
        if let Some(old) = self.native_module_exports.insert(id, reference) {
            self.unpin(&old);
        }
        Some(())
    }

    /// The cached exports for a previously-required module, if any.
    #[must_use]
    pub(crate) fn module_cache_get(
        &self,
        instance: &crate::InstanceKey,
        epoch: u64,
    ) -> Option<RawValue> {
        let entry = self.module_cache.get(instance)?;
        if entry.epoch != epoch {
            return None;
        }
        self.pinned_value(&entry.reference).ok()
    }

    /// Caches a module's exports under `id`, pinning them for the VM's lifetime
    /// or until `epoch` changes unless [`Self::clear_module_cache`] is called.
    /// Returns `None` if the pin would exceed memory.
    pub(crate) fn module_cache_set(
        &mut self,
        instance: &crate::InstanceKey,
        epoch: u64,
        exports: RawValue,
    ) -> Option<()> {
        let reference = self.pin(exports)?;
        if let Some(old) = self
            .module_cache
            .insert(instance.clone(), ModuleCacheEntry { epoch, reference })
        {
            self.unpin(&old.reference);
        }
        Some(())
    }

    /// Marks a module as in-flight. Returns false when that canonical id and
    /// source epoch are already loading in this VM.
    pub(crate) fn module_load_begin(&mut self, key: &ModuleCacheKey) -> bool {
        self.module_loading.insert(key.clone())
    }

    /// Clears an in-flight module marker after success, failure, cancellation, or
    /// deadline.
    pub(crate) fn module_load_end(&mut self, key: &ModuleCacheKey) {
        self.module_loading.remove(key);
    }

    pub(crate) fn set_print_sink(&mut self, sink: PrintSink) {
        self.print_sink = Some(sink);
    }

    pub(crate) fn replace_print_sink(&mut self, sink: Option<PrintSink>) -> Option<PrintSink> {
        std::mem::replace(&mut self.print_sink, sink)
    }

    /// Whether a `print` sink is installed — lets `print` skip formatting entirely
    /// when its output would be discarded.
    #[must_use]
    pub(crate) fn has_print_sink(&self) -> bool {
        self.print_sink.is_some()
    }

    /// Writes one formatted `print` line to the host sink, if one is installed
    /// (otherwise the output is discarded — the default no-op `print`).
    pub(crate) fn write_print_output(&mut self, bytes: &[u8]) {
        if let Some(sink) = self.print_sink.as_mut() {
            sink(bytes);
        }
    }

    #[must_use]
    pub(crate) fn runtime_compile_context(&self) -> RuntimeCompileContext {
        RuntimeCompileContext::new(
            RuntimeCompileLimits::from_effective(self.limits),
            self.cancel.clone(),
        )
    }

    /// Whether the heap's charged footprint has passed the configured cap. The
    /// dispatch safepoint raises a catchable memory error when it has, so a script
    /// growing past its share is stopped before the process backstop.
    #[must_use]
    pub fn over_memory_cap(&self) -> bool {
        self.memory_cap.is_some_and(|cap| self.meter.used() > cap)
    }

    /// Whether charging `additional` bytes now would pass the cap — a
    /// pre-allocation reserve check for a single op whose output size is
    /// data-dependent (`string.rep`, `table.concat`, `CONCAT`), so it raises
    /// *before* building a huge temporary rather than one safepoint too late.
    #[must_use]
    pub fn would_exceed_cap(&self, additional: usize) -> bool {
        self.memory_cap
            .is_some_and(|cap| self.meter.used().saturating_add(additional) > cap)
    }

    /// Whether `total` bytes would be over this heap's cap if installed as the
    /// complete heap footprint. Snapshot restore uses this before replacing the
    /// template heap, where additive checks would double-count template objects.
    #[must_use]
    pub(crate) fn total_exceeds_memory_cap(&self, total: usize) -> bool {
        self.memory_cap.is_some_and(|cap| total > cap)
    }

    /// Whether the request's cancellation token has been tripped — checked at the
    /// dispatch safepoint so a synchronous CPU loop honours cancellation too, not
    /// only a parked async await.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(crate::cancel::Cancel::is_cancelled)
    }

    /// Installs the request's cancellation token (from `Limits`).
    pub fn set_cancel(&mut self, cancel: Option<crate::cancel::Cancel>) {
        self.cancel = cancel;
    }

    /// Records a `collectgarbage("collect")` request. The collection itself runs at the
    /// next root dispatch safepoint (see [`Self::take_gc_request`]), never inline in
    /// the builtin, because only root dispatch satisfies the `collect_active`
    /// single-take-out contract.
    ///
    /// An explicit full collection forces a major: `collectgarbage("collect")` promises to
    /// reclaim *every* unreachable object, which a minor (young-only) does not.
    pub fn request_gc(&mut self) {
        self.gc_step_progress = 0;
        self.gc_step_ready = false;
        self.gc_requested = true;
        self.gc_force_major = true;
    }

    /// Advances coarse manual GC-step progress, returning `true` once a cycle is complete.
    /// Small steps schedule collection first and report completion on the following call so
    /// Lua observes the post-collection heap. A single large step completes immediately,
    /// matching Luau's one-call `collectgarbage("step", 10000)` conformance shape.
    pub fn request_gc_step(&mut self, size: usize) -> bool {
        let size = size.max(1);
        if self.gc_step_ready {
            self.gc_step_ready = false;
            return true;
        }
        if size >= GC_MANUAL_STEP_UNITS {
            self.gc_step_progress = 0;
            self.gc_requested = true;
            return true;
        }
        self.gc_step_progress = self.gc_step_progress.saturating_add(size);
        if self.gc_step_progress >= GC_MANUAL_STEP_UNITS {
            self.gc_step_progress = 0;
            self.gc_requested = true;
            self.gc_step_ready = true;
        }
        false
    }

    /// Advances host-paced manual GC-step progress, returning `true` when the
    /// caller should run one collection cycle now.
    pub(crate) fn request_host_gc_step(&mut self, size: usize) -> bool {
        let size = size.max(1);
        self.gc_step_ready = false;
        if size >= GC_MANUAL_STEP_UNITS {
            self.gc_step_progress = 0;
            return true;
        }
        self.gc_step_progress = self.gc_step_progress.saturating_add(size);
        if self.gc_step_progress >= GC_MANUAL_STEP_UNITS {
            self.gc_step_progress = 0;
            return true;
        }
        false
    }

    /// Consumes a pending `collectgarbage("collect")` request, returning whether one was
    /// set. The dispatch safepoint calls this only on root dispatch paths, so a request
    /// raised inside a coroutine stays pending until control returns there.
    pub fn take_gc_request(&mut self) -> bool {
        std::mem::take(&mut self.gc_requested)
    }

    /// Stops routine allocation-debt GC. Explicit collect/step requests are still honored.
    pub fn stop_gc(&mut self) {
        self.gc_running = false;
    }

    /// Restarts routine allocation-debt GC after `collectgarbage("stop")`.
    pub fn restart_gc(&mut self) {
        self.gc_running = true;
    }

    /// Whether routine allocation-debt GC is running.
    #[must_use]
    pub fn gc_running(&self) -> bool {
        self.gc_running
    }

    /// Whether allocation since the last collection has reached the debt threshold, so a
    /// routine `GcPolicy::Threshold` collection is due. Pacing collection to allocation —
    /// rather than only firing at the hard cap — keeps a heap with high garbage churn but a
    /// bounded live set lean, so a multi-tenant VM does not sit pinned at its ceiling. The
    /// stress policies drive their own cadence and ignore the threshold.
    #[must_use]
    pub fn gc_debt_due(&self) -> bool {
        self.gc_running
            && matches!(self.gc_policy, GcPolicy::Threshold)
            && self.meter.used() >= self.gc_threshold
    }

    /// Records that a collection just completed: advances the debt threshold to the
    /// post-collection footprint plus `GC_STEP_BYTES` and counts the cycle. The collector
    /// calls this on every successful cycle, so every collection path — the dispatch
    /// safepoint and the [`Vm::collect`](crate::Vm::collect) API — paces the next one a step
    /// of allocation ahead.
    pub fn note_collection(&mut self) {
        self.gc_threshold = self.meter.used().saturating_add(GC_STEP_BYTES);
        self.gc_cycles = self.gc_cycles.saturating_add(1);
        self.gc_step_progress = 0;
    }

    /// Whether the next collection must be a full major: one was forced (a write barrier
    /// could not record an old→young edge), allocation has grown the footprint past the
    /// major threshold (so old garbage is reclaimed and the heap compacted), or the minor
    /// safety cap is reached (so floating coroutine garbage cannot accumulate unboundedly
    /// when allocation is slow).
    #[must_use]
    pub(crate) fn gc_should_major(&self) -> bool {
        self.gc_force_major
            || self.meter.used() >= self.gc_major_threshold
            || self.gc_minors_since_major >= GC_MAJOR_SAFETY_CAP
    }

    /// Bookkeeping after a completed major: re-arm the allocation-paced major threshold from
    /// the now-compacted footprint, reset the minor budget, clear the force flag, and drop
    /// the remembered set (the major re-traced everything from real roots).
    pub(crate) fn gc_note_major(&mut self) {
        self.gc_major_threshold = self.meter.used().saturating_add(GC_MAJOR_STEP_BYTES);
        self.gc_minors_since_major = 0;
        self.gc_force_major = false;
        self.gc_remembered.clear();
    }

    /// Bookkeeping after a completed minor: spend one minor of the major budget and drop
    /// the remembered set (the minor promoted every young survivor, so the old→young edges
    /// it recorded became old→old).
    pub(crate) fn gc_note_minor(&mut self) {
        self.gc_minors_since_major = self.gc_minors_since_major.saturating_add(1);
        self.gc_remembered.clear();
    }

    /// The number of completed collection cycles over this heap's lifetime.
    #[must_use]
    pub fn gc_cycles(&self) -> u64 {
        self.gc_cycles
    }

    /// Bytes currently charged against this heap.
    #[must_use]
    pub fn used_bytes(&self) -> usize {
        self.meter.used()
    }

    /// High-water mark of charged bytes.
    #[must_use]
    pub fn peak_bytes(&self) -> usize {
        self.meter.peak()
    }

    /// A clone of the shared meter handle, for a container that joins the heap
    /// after construction (a table or thread being allocated).
    pub(crate) fn meter(&self) -> MemoryMeter {
        self.meter.clone()
    }

    /// Registers a host function, returning the `HostId` a `Proto::host`
    /// prototype references it by.
    pub(crate) fn register_host(&mut self, f: Box<dyn HostFunction>) -> HostId {
        let id = HostId(self.host_functions.len());
        self.host_functions.push(Arc::new(HostCallable::Raw(f)));
        id
    }

    /// Registers a scoped host function, returning the `HostId` a `Proto::host`
    /// prototype references it by.
    fn register_scoped_host(&mut self, f: Box<dyn ScopedHostFunction>) -> HostId {
        let id = HostId(self.host_functions.len());
        self.host_functions.push(Arc::new(HostCallable::Scoped(f)));
        id
    }

    /// Registers an async scoped host function, returning the `HostId` a
    /// `Proto::host` prototype references it by.
    fn register_async_host(&mut self, f: Box<dyn AsyncHostFunction>) -> HostId {
        let id = HostId(self.host_functions.len());
        self.host_functions.push(Arc::new(HostCallable::Async(f)));
        id
    }

    /// The registered host function behind `id`. The slot is shared, not taken
    /// out: the returned handle keeps the callable alive while a call borrows
    /// `&mut Heap`, so a host function that re-enters the VM and recursively
    /// dispatches *itself* (a bound script triggering the host call that ran it)
    /// resolves the same slot again instead of finding it empty.
    pub(crate) fn host(&self, id: HostId) -> Option<Arc<HostCallable>> {
        self.host_functions.get(id.0).cloned()
    }

    /// Registers a host function and allocates a closure that dispatches to it,
    /// the value a script calls.
    pub(crate) fn alloc_host(
        &mut self,
        f: Box<dyn HostFunction>,
    ) -> Option<RawGc<marker::Closure>> {
        let id = self.register_host(f);
        let proto = self.alloc_proto(Proto::host(id))?;
        self.alloc_closure(Closure::new(proto))
    }

    /// Registers a scoped host function and allocates the closure a script calls.
    pub fn alloc_scoped_host(
        &mut self,
        f: Box<dyn ScopedHostFunction>,
    ) -> Option<RawGc<marker::Closure>> {
        let id = self.register_scoped_host(f);
        let proto = self.alloc_proto(Proto::host(id))?;
        self.alloc_closure(Closure::new(proto))
    }

    /// Registers an async scoped host function and allocates the closure a
    /// script calls.
    pub(crate) fn alloc_async_host(
        &mut self,
        f: Box<dyn AsyncHostFunction>,
    ) -> Option<RawGc<marker::Closure>> {
        let id = self.register_async_host(f);
        let proto = self.alloc_proto(Proto::host(id))?;
        self.alloc_closure(Closure::new(proto))
    }

    /// Makes VM app data visible to scoped host functions for one VM entry.
    pub(crate) fn enter_host_app_data(&self, app_data: &RefCell<AppData>) -> HostAppDataGuard {
        let slot = NonNull::from(&*self.host_app_data);
        let previous = self
            .host_app_data
            .replace(Some(HostAppDataPtr(NonNull::from(app_data))));
        HostAppDataGuard { slot, previous }
    }

    /// Returns the VM app-data cell handle currently visible to scoped host functions.
    pub(crate) fn active_host_app_data_ptr(&self) -> Option<HostAppDataPtr> {
        self.host_app_data.get()
    }

    /// Makes borrowed host context visible to scoped host functions for one VM entry.
    pub(crate) fn enter_host_context(&self, context: &ContextSlot) -> HostContextGuard {
        let slot = NonNull::from(&*self.host_context);
        let previous = self
            .host_context
            .replace(Some(HostContextPtr(NonNull::from(context))));
        HostContextGuard { slot, previous }
    }

    /// Returns the borrowed host context handle currently visible to scoped host functions.
    pub(crate) fn active_host_context_ptr(&self) -> Option<HostContextPtr> {
        self.host_context.get()
    }

    /// The shared string metatable, if installed.
    #[must_use]
    pub fn string_metatable(&self) -> Option<RawGc<marker::Table>> {
        self.string_metatable
    }

    /// Installs the shared string metatable. Like [`set_vector_metatable`], the
    /// handle becomes a GC root, so a handle that does not resolve to a live table
    /// in this VM is rejected rather than rooted (tracing a non-resident root is
    /// unsound).
    ///
    /// Reachable from a host via `Vm::heap_mut`, not only the VM build. Treat it
    /// as trusted embedder setup: it checks liveness, but `RawGc` is not an
    /// unforgeable tenant capability.
    ///
    /// [`set_vector_metatable`]: Self::set_vector_metatable
    pub fn set_string_metatable(
        &mut self,
        metatable: RawGc<marker::Table>,
    ) -> Result<(), crate::MetatableNotResident> {
        if self.table(metatable).is_none() {
            return Err(crate::MetatableNotResident);
        }
        self.string_metatable = Some(metatable);
        Ok(())
    }

    /// The shared `vector` metatable, if installed.
    #[must_use]
    pub fn vector_metatable(&self) -> Option<RawGc<marker::Table>> {
        self.vector_metatable
    }

    /// Installs the shared `vector` metatable (the host's `lua_setmetatable` on a
    /// vector); `None` clears it. The installed handle becomes a GC root, so a
    /// handle that does not resolve to a live table in *this* VM (a dangling,
    /// stale, or cross-VM handle) is rejected rather than rooted — tracing a
    /// non-resident root would be unsound.
    ///
    /// Treat this as trusted embedder setup, not a hostile-tenant boundary. It is
    /// a liveness check; it does not reject a fabricated handle that happens to
    /// resolve to a live table, because `RawGc` is not an unforgeable capability.
    #[cfg(any(test, feature = "conformance"))]
    pub fn set_vector_metatable(
        &mut self,
        metatable: Option<RawGc<marker::Table>>,
    ) -> Result<(), crate::MetatableNotResident> {
        if let Some(handle) = metatable
            && self.table(handle).is_none()
        {
            return Err(crate::MetatableNotResident);
        }
        self.vector_metatable = metatable;
        Ok(())
    }

    /// Sets the request's instruction budget. `None` is unlimited.
    pub fn set_gas(&mut self, gas: Option<u64>) {
        self.gas = gas;
    }

    /// Installs the logical (gas-tick) deadline for this invocation.
    pub(crate) fn set_logical_deadline(&mut self, deadline: Option<u64>) {
        self.logical_deadline = deadline;
    }

    /// Whether the invocation's logical deadline has passed: the gas-spent
    /// counter is the deterministic clock, so this enforces
    /// `Deadline::Logical` exactly where wall deadlines are enforced for real
    /// requests.
    pub(crate) fn logical_deadline_exceeded(&self) -> bool {
        self.logical_deadline
            .is_some_and(|deadline| self.gas_spent >= deadline)
    }

    /// Clears the per-invocation gas-spent counter.
    pub(crate) fn reset_gas_spent(&mut self) {
        self.gas_spent = 0;
    }

    /// Gas units spent by the current or most recent invocation.
    #[must_use]
    pub fn gas_spent(&self) -> u64 {
        self.gas_spent
    }

    /// Starts a new invocation profile or clears the previous report.
    pub(crate) fn begin_gas_profile(&mut self, enabled: bool) {
        self.gas_profile = None;
        self.active_gas_profile = enabled.then(GasProfileRecorder::default);
    }

    /// Finishes the active invocation profile, if one was enabled.
    pub(crate) fn finish_gas_profile(&mut self) {
        if let Some(recorder) = self.active_gas_profile.take() {
            self.gas_profile = Some(recorder.finish(self));
        }
    }

    /// Gas attribution for the most recently completed profiled invocation.
    #[must_use]
    pub fn gas_profile(&self) -> Option<&GasProfile> {
        self.gas_profile.as_ref()
    }

    /// Whether the current invocation is recording gas attribution.
    #[must_use]
    pub(crate) fn gas_profile_active(&self) -> bool {
        self.active_gas_profile.is_some()
    }

    /// Records the Lua source site for subsequent gas charged by this
    /// instruction and any native work it enters.
    pub(crate) fn set_current_gas_site(&mut self, proto: RawGc<Proto>, pc: usize) {
        if self.active_gas_profile.is_none() {
            return;
        }
        let Some(proto_ref) = self.proto(proto) else {
            if let Some(recorder) = &mut self.active_gas_profile {
                recorder.clear_current_site();
            }
            return;
        };
        let site = GasProfileSite::new(proto, proto_ref.source, proto_ref.line(pc));
        if let Some(recorder) = &mut self.active_gas_profile {
            recorder.set_current_site(site);
        }
    }

    fn record_gas_profile(&mut self, units: u64) {
        if let Some(recorder) = &mut self.active_gas_profile {
            recorder.record(units);
        }
    }

    /// Spends one unit of the instruction budget, returning `false` when it is
    /// depleted (and leaving it depleted, so execution stays halted).
    pub fn tick_gas(&mut self) -> bool {
        if self.active_gas_profile.is_some() {
            self.tick_gas_profiled()
        } else {
            self.tick_gas_unprofiled()
        }
    }

    pub(crate) fn tick_gas_unprofiled(&mut self) -> bool {
        match &mut self.gas {
            Some(0) => false,
            Some(remaining) => {
                *remaining -= 1;
                self.gas_spent = self.gas_spent.saturating_add(1);
                true
            }
            None => true,
        }
    }

    pub(crate) fn tick_gas_profiled(&mut self) -> bool {
        if !self.tick_gas_unprofiled() {
            return false;
        }
        if self.gas.is_some() {
            self.record_gas_profile(1);
        }
        true
    }

    /// Spends `units` of the instruction budget at once, returning `false` when the
    /// budget cannot cover the whole charge (and depleting it to zero, so execution
    /// stays halted). A bulk native op whose element count is known upfront — a
    /// `table.move`/`table.unpack`/`table.concat`/`string.byte` over a large range, a
    /// `SETLIST` spread — charges the whole count before doing any work, so it either
    /// runs to completion or fails having mutated nothing. This keeps the CPU charge
    /// `O(count)` (the op runs as a single bytecode instruction, so the dispatch
    /// safepoint never fires inside it) with an all-or-nothing result, never a partial
    /// mutation.
    pub fn charge_gas(&mut self, units: u64) -> bool {
        let (spent, covered) = match &mut self.gas {
            Some(remaining) => {
                if *remaining < units {
                    let spent = *remaining;
                    self.gas_spent = self.gas_spent.saturating_add(spent);
                    *remaining = 0;
                    (spent, false)
                } else {
                    *remaining -= units;
                    self.gas_spent = self.gas_spent.saturating_add(units);
                    (units, true)
                }
            }
            None => return true,
        };
        self.record_gas_profile(spent);
        covered
    }

    /// Sets the cooperative scheduling quantum — the instruction count per slice
    /// before the driver yields the worker. `None` (or a zero slice,
    /// which would otherwise preempt every instruction forever) disables preemption.
    pub fn set_quantum(&mut self, quantum: Option<u64>) {
        self.quantum = quantum.filter(|&slice| slice != 0);
        self.quantum_remaining = self.quantum.unwrap_or(0);
    }

    /// Spends one unit of the current scheduling slice, returning `true` when the
    /// slice is exhausted (and refilling it for the next slice). `None` quantum
    /// never preempts. The async root driver and preemptible coroutine dispatch
    /// consult this; synchronous native re-entry does not.
    /// Consumes `units` instructions of the preemption quantum at once,
    /// charged by the batched dispatch safepoint. Returns `true` when the
    /// slice boundary was crossed.
    pub fn consume_quantum(&mut self, units: u32) -> bool {
        let Some(slice) = self.quantum else {
            return false;
        };
        match self.quantum_remaining.checked_sub(u64::from(units)) {
            Some(rest) => {
                self.quantum_remaining = rest;
                false
            }
            None => {
                self.quantum_remaining = slice;
                true
            }
        }
    }

    /// The object arenas.
    #[must_use]
    #[cfg(any())]
    pub(crate) fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    /// The registry of pinned values.
    #[must_use]
    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Pins `value` as a GC root in the registry (a `luaL_ref`), returning a
    /// heap-branded [`RegistryRef`] to release it with [`Heap::unpin`].
    ///
    /// # Errors
    /// Returns `None` if growing the anchor store would exceed memory.
    pub(crate) fn pin(&mut self, value: RawValue) -> Option<RegistryRef> {
        self.registry.pin(value, self.id)
    }

    /// Releases a registry-pinned value (idempotent). A ref minted by a *different* heap
    /// is ignored: unloading a module on the wrong VM must not free this VM's
    /// same-numbered slot (a cross-VM use-after-free).
    pub(crate) fn unpin(&mut self, reference: &RegistryRef) {
        if reference.heap() == self.id {
            self.registry.unpin(reference);
        }
    }

    /// Resolves a registry pin through its heap brand, slot generation, and token.
    /// Shared by the async driver and the synchronous host-result boundary, so the
    /// messages are path-neutral (a pin can now ride a synchronous return too).
    pub(crate) fn pinned_value(&self, reference: &RegistryRef) -> Result<RawValue, &'static str> {
        if reference.heap() != self.id {
            return Err("cross-VM host registry pin");
        }
        self.registry
            .get(reference)
            .ok_or("stale or forged host registry pin")
    }

    /// Roots `value` under the host-chosen name `key`, replacing and releasing any
    /// previous value at that key. The value is rooted through a registry pin, so
    /// no extra GC tracing is required.
    ///
    /// # Errors
    /// Returns `None` if the registry pin would exceed memory; the prior value (if
    /// any) is left in place.
    pub(crate) fn named_set(&mut self, key: &[u8], value: RawValue) -> Option<()> {
        let reference = self.pin(value)?;
        if let Some(old) = self.named.insert(key.to_vec(), reference) {
            self.unpin(&old);
        }
        Some(())
    }

    /// Resolves the value rooted under `key`, if any.
    #[must_use]
    pub(crate) fn named_get(&self, key: &[u8]) -> Option<RawValue> {
        let reference = self.named.get(key)?;
        self.pinned_value(reference).ok()
    }

    /// Releases the value rooted under `key`, returning whether one was present.
    pub(crate) fn named_remove(&mut self, key: &[u8]) -> bool {
        match self.named.remove(key) {
            Some(reference) => {
                self.unpin(&reference);
                true
            }
            None => false,
        }
    }

    /// Releases every named entry, unpinning each value. The per-run cleanup a host
    /// runs at the end of an execution so named state never leaks into the next run
    /// on a pooled VM. `mem::take` first so the unpin loop holds no borrow of the map.
    pub(crate) fn clear_named(&mut self) {
        for (_, reference) in std::mem::take(&mut self.named) {
            self.unpin(&reference);
        }
    }

    /// Releases every cached `require` module, unpinning its exports — the per-run
    /// reset a pooled host runs so one run's `package.loaded` does not leak into the
    /// next (modules persist for the VM lifetime otherwise).
    pub(crate) fn clear_module_cache(&mut self) {
        for (_, entry) in std::mem::take(&mut self.module_cache) {
            self.unpin(&entry.reference);
        }
        self.module_loading.clear();
    }
    pub(crate) fn begin_async_invocation(&mut self) -> u64 {
        self.next_async_invocation = self
            .next_async_invocation
            .checked_add(1)
            .expect("async invocation counter overflow");
        self.active_async_invocation = Some(self.next_async_invocation);
        self.next_async_invocation
    }
    pub(crate) fn end_async_invocation(&mut self, invocation: u64) {
        if self.active_async_invocation == Some(invocation) {
            self.active_async_invocation = None;
        }
    }

    pub(crate) fn current_async_invocation(&self) -> Option<u64> {
        self.active_async_invocation
    }

    /// Abandons coroutines touched by a fatally aborted async request. These
    /// coroutines are no longer on the active unwind stack, but may still own
    /// `RequireInfo` in-flight markers and loader pins that would block retry.
    pub(crate) fn abort_invocation_coroutines(&mut self, invocation: u64) {
        let mut threads = Vec::new();
        for index in 0..self.objects.threads.len() as u32 {
            let Some(thread) = self.objects.threads.gc_value(index) else {
                continue;
            };
            if thread.status == CoroutineStatus::Running {
                continue;
            }
            if thread.last_async_invocation == Some(invocation)
                && let Some(handle) = thread.id
            {
                threads.push(handle);
            }
        }

        for handle in threads {
            let Some(mut thread) = self.take_thread(handle) else {
                continue;
            };
            crate::coroutine::finalize_dead(self, &mut thread);
            thread.death_error = None;
            let _ = self.put_thread(handle, thread);
        }
    }

    /// A sender a `Stashed` keeps so its last clone can enqueue its pin for release
    /// (see [`Heap::drain_releases`]). Cloning is cheap and never fails.
    pub(crate) fn release_sender(&self) -> std::sync::mpsc::Sender<RegistryRef> {
        self.release_tx.clone()
    }

    /// Unpins every `Stashed` whose last clone dropped since the previous drain.
    /// Called at the start of each step so a dropped `Stashed` releases promptly,
    /// without the dropping thread needing heap access.
    pub(crate) fn drain_releases(&mut self) {
        while let Ok(reference) = self.release_rx.try_recv() {
            self.unpin(&reference);
        }
    }

    /// Checks that a host-supplied value's heap handle resolves to a live object
    /// of its own type in this heap — the accessors check the heap brand, the slot
    /// generation, and (by construction of the typed arenas) the object kind.
    /// Scalars and light userdata are always valid.
    ///
    /// This rejects stale, dangling, and cross-VM handles. It is a *liveness*
    /// check, not a *provenance* check: it cannot distinguish a handle the host was
    /// legitimately given from one the host fabricated that happens to resolve to a
    /// live object (e.g. a guessed slot). Full forgery prevention needs unforgeable
    /// handles or owned/branded result forms (tracked B2 work); this is the
    /// defense-in-depth layer at the synchronous host-result boundary. Async pinned
    /// returns are validated on materialization.
    pub(crate) fn validate_host_value(&self, value: RawValue) -> Result<(), &'static str> {
        let live = match value {
            RawValue::Nil
            | RawValue::Boolean(_)
            | RawValue::Number(_)
            | RawValue::Integer(_)
            | RawValue::Vector(_)
            | RawValue::LightUserdata { .. } => true,
            RawValue::String(handle) => self.string(handle).is_some(),
            RawValue::Table(handle) => self.table(handle).is_some(),
            RawValue::Function(handle) => self.closure(handle).is_some(),
            RawValue::Userdata(handle) => {
                handle.heap() == self.id
                    && self
                        .objects
                        .userdata
                        .get(handle.index(), handle.generation())
                        .is_some()
            }
            RawValue::Thread(handle) => self.thread(handle).is_some(),
            RawValue::Buffer(handle) => self.buffer(handle).is_some(),
        };
        if live {
            Ok(())
        } else {
            Err("host returned a forged, stale, or cross-VM heap handle")
        }
    }

    /// The running byte total charged to this heap.
    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub fn total_bytes(&self) -> usize {
        self.meter.used()
    }

    /// The live resident footprint reported to Lua's `gcinfo()` and
    /// `collectgarbage("count")`. This excludes free arena holes retained for reuse,
    /// so a successful sweep is observable even when the conservative service meter
    /// still holds high-water capacity.
    #[must_use]
    pub fn gcinfo_bytes(&self) -> usize {
        self.objects.gc_live_bytes()
    }

    /// The collector step policy baked from the seam.
    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub fn gc_policy(&self) -> GcPolicy {
        self.gc_policy
    }

    /// Whether the GC-stress policy demands a full collection at this dispatch safepoint.
    /// `CollectOnAllocation` collects on every top-level step — maximal pressure that
    /// surfaces a use-after-free immediately, since a swept-but-referenced object is
    /// reclaimed (its slot freed and soon reused) the instant it falls out of the reachable
    /// set, so the next use of a wrongly-dropped handle misbehaves or trips `validate`.
    /// `RandomizedSteps` collects on a seeded ~1-in-`GC_STRESS_STRIDE` schedule, varying
    /// *when* collection lands so a bug that only surfaces when GC interleaves at a particular
    /// point between allocation and use is exposed too — and advances its own PRNG, so it is
    /// stateful. The production `Threshold` policy never stress-collects (it reclaims on the
    /// debt threshold, the memory cap, and explicit `collectgarbage`).
    pub fn gc_stress_collect(&mut self) -> bool {
        match self.gc_policy {
            GcPolicy::CollectOnAllocation => true,
            GcPolicy::RandomizedSteps => {
                let output = pcg32_output(self.gc_rng);
                self.gc_rng = pcg32_step(self.gc_rng);
                output.is_multiple_of(GC_STRESS_STRIDE)
            }
            GcPolicy::Threshold => false,
        }
    }

    /// Interns `bytes`, returning a handle to the shared string object. Equal
    /// byte sequences share one object.
    /// The pre-interned name handle for a metamethod event.
    pub(crate) fn metamethod_name(
        &self,
        event: crate::tm::MetaEvent,
    ) -> Option<RawGc<marker::Str>> {
        self.metamethod_names[event as usize]
    }

    /// Interns `bytes`, returning the canonical shared string handle (and
    /// charging the meter for a newly created payload).
    pub fn intern_str(&mut self, bytes: &[u8]) -> Option<RawGc<marker::Str>> {
        if let Some(handle) = self.interner.lookup(bytes) {
            return Some(handle);
        }
        let (index, generation) = self
            .objects
            .strings
            .alloc(InternedString::new(bytes))
            .ok()?;
        let handle = RawGc::from_parts(index, generation, self.id);
        // The arena only counts the `InternedString` struct header; charge the byte payload
        // (the string content) explicitly. The interner accounts its key copy.
        self.meter.charge(bytes.len());
        Some(self.interner.insert(bytes, handle))
    }

    /// The string object behind a handle.
    #[must_use]
    pub fn string(&self, handle: RawGc<marker::Str>) -> Option<&InternedString> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects
            .strings
            .get(handle.index(), handle.generation())
    }

    /// Allocates a table and returns its handle. The table's internal containers
    /// are pointed at the heap's shared meter so its growth counts against the cap.
    pub fn alloc_table(&mut self, mut table: LuaTable) -> Option<RawGc<marker::Table>> {
        table.attach_hash_builder(self.hash_builder);
        table.attach_meter(self.meter.clone());
        let (index, generation) = self.objects.tables.alloc(table).ok()?;
        Some(RawGc::from_parts(index, generation, self.id))
    }

    /// The table behind a handle.
    #[must_use]
    pub fn table(&self, handle: RawGc<marker::Table>) -> Option<&LuaTable> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects.tables.get(handle.index(), handle.generation())
    }

    /// The table behind a handle, mutably.
    ///
    /// Calling this is the generational write-barrier choke point for tables: a mutation of
    /// an `Old` table may store a reference to a younger object, so the table is recorded in
    /// the remembered set (a no-op for a `Young` table, and idempotent for an already-old
    /// one). Routing every table mutation through here means no individual `set`/array-store
    /// site can forget the barrier. Remembering by index is harmless for a stale handle (the
    /// caller still gets `None` and performs no mutation).
    pub fn table_mut(&mut self, handle: RawGc<marker::Table>) -> Option<&mut LuaTable> {
        if handle.heap() != self.id {
            return None;
        }
        crate::gc::remember(self, GcRef::Table(handle.index()));
        self.objects
            .tables
            .get_mut(handle.index(), handle.generation())
    }

    /// Allocates a prototype and returns its handle.
    pub(crate) fn alloc_proto(&mut self, proto: Proto) -> Option<RawGc<Proto>> {
        // A proto owns the module's largest buffers (code/constants/lines/...);
        // the arena counts only the struct header, so charge them explicitly.
        let footprint = proto.footprint();
        let (index, generation) = self.objects.protos.alloc(proto).ok()?;
        self.meter.charge(footprint);
        Some(RawGc::from_parts(index, generation, self.id))
    }

    /// Populates a first-pass bytecode prototype and charges its immutable buffers.
    pub(crate) fn populate_proto(
        &mut self,
        handle: RawGc<Proto>,
        buffers: ProtoBuffers,
    ) -> Option<()> {
        if handle.heap() != self.id {
            return None;
        }
        let footprint = buffers.footprint();
        if self.would_exceed_cap(footprint) {
            return None;
        }
        let proto = self
            .objects
            .protos
            .get_mut(handle.index(), handle.generation())?;
        let charged = proto.populate(buffers)?;
        debug_assert_eq!(charged, footprint);
        self.meter.charge(charged);
        Some(())
    }

    /// The prototype behind a handle.
    #[must_use]
    pub(crate) fn proto(&self, handle: RawGc<Proto>) -> Option<&Proto> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects.protos.get(handle.index(), handle.generation())
    }

    /// The prototype behind a handle, mutably.
    pub(crate) fn proto_mut(&mut self, handle: RawGc<Proto>) -> Option<&mut Proto> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects
            .protos
            .get_mut(handle.index(), handle.generation())
    }

    /// Allocates a byte buffer and returns its handle. Its bytes are pointed at
    /// the heap's shared meter so they count against the cap.
    pub(crate) fn alloc_buffer(&mut self, mut buffer: LuaBuffer) -> Option<RawGc<marker::Buffer>> {
        buffer.attach_meter(self.meter.clone());
        let (index, generation) = self.objects.buffers.alloc(buffer).ok()?;
        Some(RawGc::from_parts(index, generation, self.id))
    }

    /// The buffer behind a handle.
    #[must_use]
    pub(crate) fn buffer(&self, handle: RawGc<marker::Buffer>) -> Option<&LuaBuffer> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects
            .buffers
            .get(handle.index(), handle.generation())
    }

    /// The buffer behind a handle, mutably.
    pub(crate) fn buffer_mut(&mut self, handle: RawGc<marker::Buffer>) -> Option<&mut LuaBuffer> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects
            .buffers
            .get_mut(handle.index(), handle.generation())
    }

    /// Allocates a host userdata and returns its handle. Its boxed payload is
    /// pointed at the heap's shared meter so it counts against the cap (and is
    /// released by the userdata's `Drop` when the GC sweeps it).
    pub(crate) fn alloc_userdata(
        &mut self,
        mut userdata: LuaUserdata,
    ) -> Option<RawGc<marker::Userdata>> {
        userdata.attach_meter(self.meter.clone());
        let (index, generation) = self.objects.userdata.alloc(userdata).ok()?;
        Some(RawGc::from_parts(index, generation, self.id))
    }

    /// The host userdata behind a handle. Userdata carry no traced heap
    /// references and mutate only through their interior borrow cell, so there
    /// is no `_mut` companion (and no write barrier to route).
    #[must_use]
    pub(crate) fn userdata(&self, handle: RawGc<marker::Userdata>) -> Option<&LuaUserdata> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects
            .userdata
            .get(handle.index(), handle.generation())
    }

    /// Registers a built host type's runtime entry; its index becomes the
    /// `type_index` new instances carry.
    pub(crate) fn register_host_type(&mut self, runtime: crate::host_type::HostTypeRuntime) {
        self.host_types.push(runtime);
    }

    /// The registered host types, indexed by `LuaUserdata::type_index`.
    #[must_use]
    pub(crate) fn host_types(&self) -> &[crate::host_type::HostTypeRuntime] {
        &self.host_types
    }

    /// The registry entry (and its index) for the host type registered under
    /// the Rust type `type_id`, if any.
    #[must_use]
    pub(crate) fn host_type_for(
        &self,
        type_id: std::any::TypeId,
    ) -> Option<(u32, &crate::host_type::HostTypeRuntime)> {
        self.host_types
            .iter()
            .position(|entry| entry.type_id == type_id)
            .map(|index| (index as u32, &self.host_types[index]))
    }

    /// The shared metatable for a host userdata value — its registered type's
    /// metatable, resolved through the type index the instance carries.
    #[must_use]
    pub(crate) fn userdata_metatable(
        &self,
        handle: RawGc<marker::Userdata>,
    ) -> Option<RawGc<marker::Table>> {
        let userdata = self.userdata(handle)?;
        self.host_types
            .get(userdata.type_index() as usize)
            .map(|entry| entry.metatable)
    }

    /// Allocates a closure and returns its handle.
    pub(crate) fn alloc_closure(&mut self, closure: Closure) -> Option<RawGc<marker::Closure>> {
        let (index, generation) = self.objects.closures.alloc(closure).ok()?;
        Some(RawGc::from_parts(index, generation, self.id))
    }

    /// Allocates an engine builtin as a callable closure over a native prototype.
    pub fn alloc_builtin(&mut self, builtin: Builtin) -> Option<RawGc<marker::Closure>> {
        let proto = self.alloc_proto(Proto::native(builtin))?;
        self.alloc_closure(Closure::new(proto))
    }

    /// The closure behind a handle.
    #[must_use]
    pub(crate) fn closure(&self, handle: RawGc<marker::Closure>) -> Option<&Closure> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects
            .closures
            .get(handle.index(), handle.generation())
    }

    /// The closure behind a handle, mutably (the interpreter binds upvalue cells
    /// here as `NEWCLOSURE` runs).
    ///
    /// A closure carries heap references (its prototype and upvalue cells), so this is a
    /// generational write-barrier choke like [`table_mut`](Self::table_mut) /
    /// [`upval_mut`](Self::upval_mut): mutating an `Old` closure may store a younger upvalue
    /// reference, so the closure is remembered. The interpreter only mutates a freshly-
    /// allocated (`Young`) closure, where this is a no-op, but the barrier keeps the public
    /// API (`Vm::heap_mut`, with `Closure`/`UpVal` re-exported) sound for an embedder that
    /// pushes an upvalue into an already-old closure.
    pub(crate) fn closure_mut(&mut self, handle: RawGc<marker::Closure>) -> Option<&mut Closure> {
        if handle.heap() != self.id {
            return None;
        }
        crate::gc::remember(self, GcRef::Closure(handle.index()));
        self.objects
            .closures
            .get_mut(handle.index(), handle.generation())
    }

    /// Allocates an upvalue cell and returns its handle.
    pub(crate) fn alloc_upval(&mut self, upval: UpVal) -> Option<RawGc<UpVal>> {
        let (index, generation) = self.objects.upvals.alloc(upval).ok()?;
        Some(RawGc::from_parts(index, generation, self.id))
    }

    /// The upvalue cell behind a handle.
    #[must_use]
    pub(crate) fn upval(&self, handle: RawGc<UpVal>) -> Option<&UpVal> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects.upvals.get(handle.index(), handle.generation())
    }

    /// The upvalue cell behind a handle, mutably (a `SETUPVAL` to a closed cell,
    /// or closing an open cell on scope exit).
    ///
    /// Like [`table_mut`](Self::table_mut), this is the generational write-barrier choke for
    /// upvalue cells: closing or reassigning an `Old` cell may store a younger value, so the
    /// cell is remembered (a no-op for a `Young` cell). An open cell aliases a thread
    /// register, which is covered separately (every thread is a minor root); the barrier
    /// here covers the closed-value store.
    pub(crate) fn upval_mut(&mut self, handle: RawGc<UpVal>) -> Option<&mut UpVal> {
        if handle.heap() != self.id {
            return None;
        }
        crate::gc::remember(self, GcRef::UpVal(handle.index()));
        self.objects
            .upvals
            .get_mut(handle.index(), handle.generation())
    }

    /// Allocates a thread object and returns its handle. The main thread and
    /// coroutines both use arena-resident thread handles for open upvalue owners.
    pub(crate) fn alloc_thread(&mut self, mut thread: Thread) -> Option<RawGc<marker::Thread>> {
        thread.attach_meter(&self.meter);
        let (index, generation) = self.objects.threads.alloc(thread).ok()?;
        Some(RawGc::from_parts(index, generation, self.id))
    }

    /// The thread (coroutine) behind a handle.
    #[must_use]
    pub(crate) fn thread(&self, handle: RawGc<marker::Thread>) -> Option<&Thread> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects
            .threads
            .get(handle.index(), handle.generation())
    }

    /// The thread (coroutine) behind a handle, mutably. Use the crate-internal
    /// `take_thread`/`put_thread` when execution needs to move a thread out of its
    /// arena slot.
    pub(crate) fn thread_mut(&mut self, handle: RawGc<marker::Thread>) -> Option<&mut Thread> {
        if handle.heap() != self.id {
            return None;
        }
        self.objects
            .threads
            .get_mut(handle.index(), handle.generation())
    }

    /// Takes an arena-resident thread out for execution, leaving the reserved
    /// placeholder in its slot and recording the take-out depth for GC safety.
    pub(crate) fn take_thread(&mut self, handle: RawGc<marker::Thread>) -> Option<Thread> {
        let thread = {
            let slot = self.thread_mut(handle)?;
            if slot.id != Some(handle) {
                return None;
            }
            std::mem::take(slot)
        };
        self.taken_out_threads = self
            .taken_out_threads
            .checked_add(1)
            .expect("thread take-out depth overflow");
        Some(thread)
    }

    /// Restores a previously taken-out thread to its reserved arena slot.
    pub(crate) fn put_thread(&mut self, handle: RawGc<marker::Thread>, thread: Thread) -> bool {
        assert!(
            self.taken_out_threads > 0,
            "put_thread called without a matching take_thread"
        );
        {
            let Some(slot) = self.thread_mut(handle) else {
                return false;
            };
            *slot = thread;
        }
        self.taken_out_threads -= 1;
        true
    }

    /// Number of arena-resident threads currently held outside their arena slots.
    #[must_use]
    pub(crate) fn taken_out_thread_count(&self) -> u32 {
        self.taken_out_threads
    }

    /// Marks a borrowed scope step as active, returning `false` if one already is
    /// (the re-entry guard). The caller must pair a `true` result with
    /// [`Heap::exit_scope`].
    pub(crate) fn try_enter_scope(&mut self) -> bool {
        if self.scope_active {
            return false;
        }
        self.scope_active = true;
        true
    }

    /// Whether a borrowed scope is currently active on this heap.
    pub(crate) fn scope_active(&self) -> bool {
        self.scope_active
    }

    /// Clears the active-scope flag set by [`Heap::try_enter_scope`].
    pub(crate) fn exit_scope(&mut self) {
        self.scope_active = false;
    }
}

#[cfg(any())]
mod tests {
    use super::*;

    fn heap() -> Heap {
        Heap::new(
            HeapId(1),
            AmbientConfig {
                hash_seed: 0,
                prng_seed: 0,
                gc_policy: GcPolicy::Threshold,
            },
        )
    }

    #[test]
    fn memory_meter_peak_preserves_high_water_after_release() {
        let meter = MemoryMeter::default();

        meter.charge(128);
        meter.adjust(128, 32);

        assert_eq!(meter.used(), 32);
        assert_eq!(meter.peak(), 128);
        assert!(meter.peak() > meter.used());
    }

    #[test]
    fn interning_dedups_equal_content() {
        let mut h = heap();
        let a = h.intern_str(b"hello").unwrap();
        let b = h.intern_str(b"hello").unwrap();
        let c = h.intern_str(b"world").unwrap();
        assert_eq!(a.index(), b.index());
        assert_ne!(a.index(), c.index());
        assert_eq!(h.string(a).unwrap().bytes(), b"hello");
    }

    #[test]
    fn alloc_then_deref_roundtrips() {
        let mut h = heap();
        let t = h.alloc_table(LuaTable::new()).unwrap();
        assert!(h.table(t).is_some());
    }

    #[test]
    fn registry_pin_generation_rejects_stale_reused_slots() {
        let mut h = heap();
        let first = h.pin(RawValue::Number(1.0)).expect("pin first");
        assert_eq!(h.pinned_value(&first), Ok(RawValue::Number(1.0)));
        h.unpin(&first);
        assert_eq!(
            h.pinned_value(&first),
            Err("stale or forged host registry pin"),
            "unpinning makes the old token stale"
        );

        let second = h.pin(RawValue::Number(2.0)).expect("pin second");
        assert_eq!(
            second.slot(),
            first.slot(),
            "registry reuses the freed slot"
        );
        assert_ne!(
            second.generation(),
            first.generation(),
            "the reused slot has a fresh generation"
        );
        assert_eq!(
            h.pinned_value(&first),
            Err("stale or forged host registry pin"),
            "the old generation still cannot resolve after reuse"
        );
        assert_eq!(h.pinned_value(&second), Ok(RawValue::Number(2.0)));
    }

    #[test]
    fn registry_pin_identity_rejects_forged_token() {
        let mut h = heap();
        let pinned = h.pin(RawValue::Number(1.0)).expect("pin");
        let forged = RegistryRef::from_parts(pinned.slot(), pinned.generation(), h.id);

        assert_eq!(
            h.pinned_value(&forged),
            Err("stale or forged host registry pin"),
            "matching numeric parts are not enough to forge a registry token"
        );
        assert_eq!(h.pinned_value(&pinned), Ok(RawValue::Number(1.0)));
    }

    #[test]
    fn thread_take_out_depth_tracks_execution_slots() {
        let mut h = heap();
        let t1 = h.alloc_thread(Thread::new()).unwrap();
        let t2 = h.alloc_thread(Thread::new()).unwrap();
        h.thread_mut(t1).unwrap().id = Some(t1);
        h.thread_mut(t2).unwrap().id = Some(t2);
        assert_eq!(h.taken_out_thread_count(), 0);

        let thread1 = h.take_thread(t1).expect("take first thread");
        assert_eq!(h.taken_out_thread_count(), 1);
        assert!(
            h.take_thread(t1).is_none(),
            "a placeholder slot cannot be taken out again"
        );
        assert_eq!(h.taken_out_thread_count(), 1);
        let thread2 = h.take_thread(t2).expect("take second thread");
        assert_eq!(h.taken_out_thread_count(), 2);

        assert!(h.put_thread(t2, thread2));
        assert_eq!(h.taken_out_thread_count(), 1);
        assert!(h.put_thread(t1, thread1));
        assert_eq!(h.taken_out_thread_count(), 0);
    }

    #[test]
    fn gc_request_flag_round_trips() {
        // `collectgarbage("collect")` sets the flag; a root safepoint consumes it
        // exactly once. A nested safepoint never calls `take_gc_request`, so the
        // flag persists until a root path reaches it.
        let mut h = heap();
        assert!(!h.take_gc_request(), "no request is pending initially");
        h.request_gc();
        assert!(h.take_gc_request(), "a pending request is observed once");
        assert!(
            !h.take_gc_request(),
            "and is consumed, so it fires at most one collection"
        );
    }

    #[test]
    fn stale_generation_is_rejected() {
        let mut h = heap();
        let mut bad = h.alloc_table(LuaTable::new()).unwrap();
        // Forge a handle with a wrong generation.
        bad = RawGc::from_parts(bad.index(), bad.generation().wrapping_add(1), h.id);
        assert!(h.table(bad).is_none());
    }

    #[test]
    fn compaction_persists_generations_so_a_regrown_slot_cannot_alias() {
        // The sweep truncates a reclaimed arena tail to release its capacity, but the
        // per-index generation must survive in `gens` so that a slot regrown at a reclaimed
        // index does not validate a stale handle to its previous occupant — the
        // use-after-free the side-table generation vector exists to prevent.
        let mut arena: Arena<u64> = Arena::new();
        let (i0, g0) = arena.alloc(100).unwrap(); // first slot: index 0, generation 0 (the danger)
        let (i1, _) = arena.alloc(200).unwrap();
        let (i2, _) = arena.alloc(300).unwrap();
        assert_eq!((i0, g0), (0, 0));
        assert_eq!(arena.get(i0, g0), Some(&100));

        // Sweep: free every slot, then compact (truncates the now-empty tail).
        arena.free(i0);
        arena.free(i1);
        arena.free(i2);
        arena.gc_compact();
        assert_eq!(arena.get(i0, g0), None, "a freed slot's handle is stale");

        // Regrow: a new object reuses index 0 (lowest-first), but at a generation past the old
        // occupant's, so the stale (0, 0) handle stays rejected and only the new one validates.
        let (j, gj) = arena.alloc(999).unwrap();
        assert_eq!(j, 0, "lowest-first reuse hands back the reclaimed index 0");
        assert_ne!(gj, g0, "but at a fresh generation");
        assert_eq!(
            arena.get(i0, g0),
            None,
            "the stale handle must not alias the regrown slot"
        );
        assert_eq!(arena.get(j, gj), Some(&999), "the regrown handle is valid");
    }

    #[test]
    fn cross_vm_handle_is_rejected() {
        let mut a = heap();
        let other = Heap::new(
            HeapId(2),
            AmbientConfig {
                hash_seed: 0,
                prng_seed: 0,
                gc_policy: GcPolicy::Threshold,
            },
        );
        let handle = a.alloc_table(LuaTable::new()).unwrap();
        assert!(other.table(handle).is_none());
    }
}
