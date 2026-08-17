use std::{
    any::Any,
    collections::HashSet,
    error::Error as StdError,
    fmt,
    marker::PhantomData,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use ruau_bytecode::BytecodeChunk;
use ruau_source::{ModuleId, Source};
use ruau_surface::{
    PrepareGraphError, PrepareOptions, PreparedGraph, PreparedLoadError, Surface, VmConfig,
};
use ruau_vm::{
    BindChunkEnvironmentError, CallOptions, ExecError, Function, IntoStash, LoadError,
    LoadedModule, ModuleDomainId, MultiValue, RuntimeCompiler, RuntimeError, Scope, ScopedValue,
    StashedClosure, StashedTable, StashedValue, Table, ValueSnapshot, Vm, VmBuildError,
};

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

fn with_optional_runtime_compiler<R>(
    vm: &mut Vm,
    compiler: Option<Arc<dyn RuntimeCompiler>>,
    operation: impl FnOnce(&mut Vm) -> R,
) -> R {
    match compiler {
        Some(compiler) => vm.with_runtime_compiler(compiler, operation),
        None => operation(vm),
    }
}

/// How a compiled root is identified when loaded into a retained runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadTarget {
    /// Load the chunk with a traceback/debug chunk name.
    Named(Vec<u8>),
    /// Load the chunk as the body for a concrete module id.
    ModuleId(ModuleId),
    /// Load the chunk as a module id with a separate traceback/debug name.
    NamedModule {
        /// Runtime requester identity for relative `require`.
        module_id: ModuleId,
        /// Human-facing chunk name for tracebacks and debug locations.
        chunk_name: Vec<u8>,
    },
}

impl LoadTarget {
    /// Builds a named chunk load target.
    #[must_use]
    pub fn named(chunk_name: impl Into<Vec<u8>>) -> Self {
        Self::Named(chunk_name.into())
    }

    /// Builds a concrete module-id load target.
    #[must_use]
    pub fn module_id(module_id: ModuleId) -> Self {
        Self::ModuleId(module_id)
    }

    /// Builds a module-id target with a separate traceback/debug name.
    #[must_use]
    pub fn named_module(module_id: ModuleId, chunk_name: impl Into<Vec<u8>>) -> Self {
        Self::NamedModule {
            module_id,
            chunk_name: chunk_name.into(),
        }
    }

    fn load(&self, vm: &mut Vm, chunk: &BytecodeChunk) -> Result<LoadedModule, LoadError> {
        match self {
            Self::Named(chunk_name) => vm.load_named(chunk, chunk_name),
            Self::ModuleId(module_id) => vm.load_module(chunk, module_id.clone()),
            Self::NamedModule {
                module_id,
                chunk_name,
            } => vm.load_named_module(chunk, module_id.clone(), chunk_name),
        }
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct HandleKey {
    owner: u64,
    slot: usize,
    generation: u64,
    source_epoch: u64,
}

impl HandleKey {
    const fn new(owner: u64, slot: usize, generation: u64, source_epoch: u64) -> Self {
        Self {
            owner,
            slot,
            generation,
            source_epoch,
        }
    }
}

macro_rules! retained_handle {
    ($name:ident, $marker:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(Clone, Copy, Eq, Hash, PartialEq)]
        pub struct $name {
            key: HandleKey,
            _marker: PhantomData<fn() -> $marker>,
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .field("source_epoch", &self.key.source_epoch)
                    .finish_non_exhaustive()
            }
        }

        impl $name {
            fn new(owner: u64, slot: usize, generation: u64, source_epoch: u64) -> Self {
                Self {
                    key: HandleKey::new(owner, slot, generation, source_epoch),
                    _marker: PhantomData,
                }
            }

            /// Module-source epoch in which this handle was created.
            #[must_use]
            pub const fn source_epoch(self) -> u64 {
                self.key.source_epoch
            }
        }
    };
}

struct ValueMarker;
struct TableMarker;

retained_handle!(
    ValueHandle,
    ValueMarker,
    "Typed handle for an arbitrary registry-rooted Luau value."
);
retained_handle!(
    TableHandle,
    TableMarker,
    "Typed handle for a registry-rooted Luau table."
);

#[allow(unnameable_types)]
mod handle_sealed {
    pub trait Sealed {}
    pub trait RetainSealed {}
}

/// A typed retained-runtime handle accepted by [`Runtime::get`] and
/// [`Runtime::release`].
pub trait Handle: handle_sealed::Sealed {
    /// Borrowed VM value made available while the runtime scope is open.
    type Scoped<'scope>;

    #[doc(hidden)]
    fn get<R, F>(
        self,
        runtime: &mut Runtime,
        options: &CallOptions,
        body: F,
    ) -> Result<R, LifecycleError>
    where
        R: IntoStash,
        F: for<'scope> FnOnce(&Scope<'scope>, Self::Scoped<'scope>) -> Result<R, RuntimeError>;

    #[doc(hidden)]
    fn release(self, runtime: &mut Runtime) -> Result<(), LifecycleError>;
}

/// A VM stash that can be moved into a retained [`Runtime`].
pub trait Retain: handle_sealed::RetainSealed {
    /// Typed handle returned for this stash kind.
    type Handle;

    #[doc(hidden)]
    fn retain(self, runtime: &mut Runtime) -> Self::Handle;
}

/// Typed handle for a loaded retained graph or compiled root.
#[derive(Clone)]
pub struct RootHandle {
    key: HandleKey,
    function: ResolvableFunction,
}

impl fmt::Debug for RootHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RootHandle")
            .field("source_epoch", &self.key.source_epoch)
            .finish_non_exhaustive()
    }
}

/// Handle for one retained runtime module-cache domain.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ModuleDomainHandle {
    key: HandleKey,
    id: ModuleDomainId,
}

/// Handle for one detached retained invocation.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct InvocationHandle {
    key: HandleKey,
}

impl fmt::Debug for InvocationHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InvocationHandle")
            .field("source_epoch", &self.key.source_epoch)
            .finish_non_exhaustive()
    }
}

impl InvocationHandle {
    fn new(owner: u64, slot: usize, generation: u64, source_epoch: u64) -> Self {
        Self {
            key: HandleKey::new(owner, slot, generation, source_epoch),
        }
    }

    /// Module-source epoch in which this invocation was created.
    #[must_use]
    pub const fn source_epoch(self) -> u64 {
        self.key.source_epoch
    }
}

impl fmt::Debug for ModuleDomainHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModuleDomainHandle")
            .field("source_epoch", &self.key.source_epoch)
            .finish_non_exhaustive()
    }
}

impl ModuleDomainHandle {
    fn new(
        slot: usize,
        generation: u64,
        source_epoch: u64,
        owner: u64,
        id: ModuleDomainId,
    ) -> Self {
        Self {
            key: HandleKey::new(owner, slot, generation, source_epoch),
            id,
        }
    }

    /// Module-source epoch in which this domain was created.
    #[must_use]
    pub const fn source_epoch(self) -> u64 {
        self.key.source_epoch
    }
}

impl RootHandle {
    fn new(
        owner: u64,
        slot: usize,
        generation: u64,
        source_epoch: u64,
        function: Weak<StashedClosure>,
        current_epoch: Arc<AtomicU64>,
    ) -> Self {
        Self {
            key: HandleKey::new(owner, slot, generation, source_epoch),
            function: ResolvableFunction {
                function,
                current_epoch,
            },
        }
    }

    /// Module-source epoch in which this handle was created.
    #[must_use]
    pub const fn source_epoch(&self) -> u64 {
        self.key.source_epoch
    }

    /// Resolves this retained root's main function inside an existing VM scope.
    ///
    /// # Errors
    /// Returns a stale-handle error after unload or source invalidation, or a
    /// runtime error when `scope` belongs to a different VM.
    pub fn resolve<'s>(&self, scope: &Scope<'s>) -> Result<Function<'s>, LifecycleError> {
        self.function.resolve(self.key, HandleKind::Root, scope)
    }
}

/// Typed handle for a registry-rooted Luau function.
#[derive(Clone)]
pub struct FunctionHandle {
    key: HandleKey,
    function: ResolvableFunction,
}

impl fmt::Debug for FunctionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FunctionHandle")
            .field("source_epoch", &self.key.source_epoch)
            .finish_non_exhaustive()
    }
}

impl FunctionHandle {
    fn new(
        owner: u64,
        slot: usize,
        generation: u64,
        source_epoch: u64,
        function: Weak<StashedClosure>,
        current_epoch: Arc<AtomicU64>,
    ) -> Self {
        Self {
            key: HandleKey::new(owner, slot, generation, source_epoch),
            function: ResolvableFunction {
                function,
                current_epoch,
            },
        }
    }

    /// Module-source epoch in which this handle was created.
    #[must_use]
    pub const fn source_epoch(&self) -> u64 {
        self.key.source_epoch
    }

    /// Resolves this retained function inside an existing VM scope.
    ///
    /// # Errors
    /// Returns a stale-handle error after release or source invalidation, or a
    /// runtime error when `scope` belongs to a different VM.
    pub fn resolve<'s>(&self, scope: &Scope<'s>) -> Result<Function<'s>, LifecycleError> {
        self.function.resolve(self.key, HandleKind::Function, scope)
    }
}

#[derive(Clone)]
struct ResolvableFunction {
    function: Weak<StashedClosure>,
    current_epoch: Arc<AtomicU64>,
}

impl ResolvableFunction {
    fn resolve<'s>(
        &self,
        key: HandleKey,
        kind: HandleKind,
        scope: &Scope<'s>,
    ) -> Result<Function<'s>, LifecycleError> {
        let current_epoch = self.current_epoch.load(Ordering::Acquire);
        let function = self
            .function
            .upgrade()
            .filter(|_| key.source_epoch == current_epoch);
        let function = function.ok_or(LifecycleError::StaleHandle {
            kind,
            handle_epoch: key.source_epoch,
            current_epoch,
        })?;
        scope
            .fetch_function(&function)
            .map_err(LifecycleError::Runtime)
    }
}

/// Handle category reported by [`LifecycleError::StaleHandle`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleKind {
    /// Loaded root.
    Root,
    /// Arbitrary stashed value.
    Value,
    /// Stashed table.
    Table,
    /// Stashed function.
    Function,
    /// Module-cache domain.
    ModuleDomain,
    /// Detached invocation.
    Invocation,
}

/// Cache state released with one module domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModuleDomainRelease {
    /// Completed module exports released.
    pub cached_modules: usize,
    /// In-flight module markers released.
    pub in_flight_modules: usize,
}

/// Resource usage from one detached invocation poll.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InvocationPollUsage {
    /// Gas consumed by the matching VM segment.
    pub gas_spent: u64,
}

/// Outcome and resource usage from one detached invocation poll.
#[derive(Debug)]
pub struct InvocationStep<R, E> {
    /// Poll outcome from the matching VM segment.
    pub poll: Poll<Result<R, InvocationError<E>>>,
    /// Resource usage from the matching VM segment.
    pub usage: InvocationPollUsage,
}

/// Failure from a typed detached invocation poll.
#[derive(Debug)]
pub enum InvocationError<E> {
    /// Retained lifecycle or VM execution failed.
    Lifecycle(LifecycleError),
    /// The successful-result decoder failed.
    Completion(E),
}

/// Values released when a retained runtime crosses a source generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Invalidation {
    /// Previous source epoch.
    pub previous_epoch: u64,
    /// Current source epoch after invalidation.
    pub current_epoch: u64,
    /// Loaded roots released.
    pub roots: usize,
    /// Arbitrary value stashes released.
    pub values: usize,
    /// Table stashes released.
    pub tables: usize,
    /// Function stashes released.
    pub functions: usize,
    /// Module-cache domains released.
    pub module_domains: usize,
    /// Detached invocations released.
    pub invocations: usize,
}

/// Failure from retained-runtime state management or VM execution.
#[derive(Debug)]
pub enum LifecycleError {
    /// A released, unloaded, or previous-epoch handle was used.
    StaleHandle {
        /// Kind of handle that failed validation.
        kind: HandleKind,
        /// Epoch recorded on the handle.
        handle_epoch: u64,
        /// Runtime's current epoch.
        current_epoch: u64,
    },
    /// A live retained object still uses the requested handle.
    InUse {
        /// Kind of handle that is still in use.
        kind: HandleKind,
    },
    /// The requested handle names permanent runtime state.
    PermanentHandle {
        /// Kind of permanent handle.
        kind: HandleKind,
    },
    /// A compiled chunk could not be loaded.
    Load(LoadError),
    /// A prepared graph no longer matches this runtime or could not be loaded.
    PreparedLoad(PreparedLoadError),
    /// A loaded root could not be isolated in its own chunk environment.
    BindEnvironment(BindChunkEnvironmentError),
    /// The VM reported execution or result-marshaling failure.
    Exec(ExecError),
    /// A borrowed scope operation failed.
    Runtime(RuntimeError),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleHandle {
                kind,
                handle_epoch,
                current_epoch,
            } => write!(
                formatter,
                "stale retained {kind:?} handle from source epoch {handle_epoch}; current epoch is {current_epoch}"
            ),
            Self::InUse { kind } => {
                write!(formatter, "retained {kind:?} handle is still in use")
            }
            Self::PermanentHandle { kind } => {
                write!(formatter, "retained {kind:?} handle is permanent")
            }
            Self::Load(error) => write!(formatter, "retained root load failed: {error}"),
            Self::PreparedLoad(error) => {
                write!(formatter, "prepared retained root load failed: {error}")
            }
            Self::BindEnvironment(error) => {
                write!(
                    formatter,
                    "retained root environment binding failed: {error}"
                )
            }
            Self::Exec(error) => write!(formatter, "retained execution failed: {error}"),
            Self::Runtime(error) => write!(formatter, "retained scope step failed: {error}"),
        }
    }
}

impl StdError for LifecycleError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::StaleHandle { .. } | Self::InUse { .. } | Self::PermanentHandle { .. } => None,
            Self::Load(error) => Some(error),
            Self::PreparedLoad(error) => Some(error),
            Self::BindEnvironment(error) => Some(error),
            Self::Exec(error) => Some(error),
            Self::Runtime(error) => Some(error),
        }
    }
}

impl<E: fmt::Display> fmt::Display for InvocationError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lifecycle(error) => write!(formatter, "{error}"),
            Self::Completion(error) => write!(formatter, "retained completion failed: {error}"),
        }
    }
}

impl<E: StdError + 'static> StdError for InvocationError<E> {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Lifecycle(error) => Some(error),
            Self::Completion(error) => Some(error),
        }
    }
}

impl From<RuntimeError> for LifecycleError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

/// Lock-free retained execution core over one validated surface and VM.
///
/// The host chooses synchronization and worker ownership. This type requires
/// `&mut self`, performs no background work, and owns no application callback
/// ids or registries. Source-epoch changes invalidate loaded roots and every
/// stashed handle; they never reload a root implicitly.
pub struct Runtime {
    id: u64,
    surface: Arc<Surface>,
    vm: Vm,
    source_epoch: u64,
    current_epoch: Arc<AtomicU64>,
    next_module_domain_id: u64,
    default_module_domain: ModuleDomainHandle,
    module_domains: GenerationalArena<RetainedModuleDomain>,
    invocations: GenerationalArena<RetainedInvocation>,
    roots: GenerationalArena<RetainedRoot>,
    values: GenerationalArena<StashedValue>,
    tables: GenerationalArena<StashedTable>,
    functions: GenerationalArena<RetainedFunction>,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("id", &self.id)
            .field("source_epoch", &self.source_epoch)
            .field("execution_count", &self.vm.execution_count())
            .field("heap_used_bytes", &self.vm.heap_used_bytes())
            .field("module_domains", &self.module_domains.len())
            .field("invocations", &self.invocations.len())
            .field("roots", &self.roots.len())
            .field("values", &self.values.len())
            .field("tables", &self.tables.len())
            .field("functions", &self.functions.len())
            .finish_non_exhaustive()
    }
}

struct RetainedRoot {
    module: LoadedModule,
    _function: Arc<StashedClosure>,
    domain: ModuleDomainHandle,
    runtime_compiler: Option<Arc<dyn RuntimeCompiler>>,
}

struct RetainedModuleDomain {
    id: ModuleDomainId,
    permanent: bool,
    roots: usize,
    invocations: usize,
}

struct RetainedInvocation {
    invocation: ruau_vm::DetachedInvocation,
    domain: ModuleDomainHandle,
    runtime_compiler: Option<Arc<dyn RuntimeCompiler>>,
}

struct RetainedFunction {
    function: Arc<StashedClosure>,
}

impl Runtime {
    /// Builds a retained runtime over a validated surface.
    ///
    /// # Errors
    /// Returns [`VmBuildError`] when the surface VM cannot be built.
    pub fn new(surface: Surface, config: &VmConfig) -> Result<Self, VmBuildError> {
        Self::with_shared_surface(Arc::new(surface), config)
    }

    /// Builds a retained runtime sharing an existing surface allocation.
    ///
    /// # Errors
    /// Returns [`VmBuildError`] when the surface VM cannot be built.
    pub fn with_shared_surface(
        surface: Arc<Surface>,
        config: &VmConfig,
    ) -> Result<Self, VmBuildError> {
        let id = NEXT_RUNTIME_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("retained runtime counter overflow");
        let source_epoch = surface_source_epoch(&surface);
        let vm = surface.vm_builder(config).build()?;
        let mut module_domains = GenerationalArena::default();
        let (slot, generation) = module_domains.insert(RetainedModuleDomain {
            id: ModuleDomainId::DEFAULT,
            permanent: true,
            roots: 0,
            invocations: 0,
        });
        let default_module_domain =
            ModuleDomainHandle::new(slot, generation, source_epoch, id, ModuleDomainId::DEFAULT);
        Ok(Self {
            id,
            surface,
            vm,
            source_epoch,
            current_epoch: Arc::new(AtomicU64::new(source_epoch)),
            next_module_domain_id: 1,
            default_module_domain,
            module_domains,
            invocations: GenerationalArena::default(),
            roots: GenerationalArena::default(),
            values: GenerationalArena::default(),
            tables: GenerationalArena::default(),
            functions: GenerationalArena::default(),
        })
    }

    /// Validated surface used by this runtime.
    #[must_use]
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Source epoch currently associated with all live handles.
    #[must_use]
    pub const fn source_epoch(&self) -> u64 {
        self.source_epoch
    }

    /// Whether the surface's source has advanced beyond this runtime's handles.
    #[must_use]
    pub fn source_epoch_changed(&self) -> bool {
        surface_source_epoch(&self.surface) != self.source_epoch
    }

    /// Host-initiated VM invocations performed over this runtime's lifetime.
    #[must_use]
    pub const fn execution_count(&self) -> u64 {
        self.vm.execution_count()
    }

    /// Bytes currently occupied by the retained VM heap.
    ///
    /// This is an observational metric for host diagnostics and regression
    /// tests. Boundary collection remains automatic at retained entrypoints;
    /// reading the metric does not trigger collection or otherwise mutate the
    /// runtime.
    #[must_use]
    pub fn heap_used_bytes(&self) -> usize {
        self.vm.heap_used_bytes()
    }

    /// Gas spent by the most recent retained-runtime invocation.
    ///
    /// The counter resets at each entrypoint and remains available after both
    /// successful and failed execution for diagnostics and regression tests.
    #[must_use]
    pub fn gas_spent(&self) -> u64 {
        self.vm.gas_spent()
    }

    /// Checks and prepares a source graph using the retained surface.
    ///
    /// # Errors
    /// Returns [`PrepareGraphError`] for rejected diagnostics or compilation.
    pub fn prepare_ready(
        &self,
        source: Source,
        options: PrepareOptions,
    ) -> Result<PreparedGraph, PrepareGraphError> {
        self.surface
            .prepare_graph_ready_with_options(source, options)
    }

    /// Asynchronously checks and prepares a source graph.
    ///
    /// # Errors
    /// Returns [`PrepareGraphError`] for rejected diagnostics or compilation.
    pub async fn prepare(
        &self,
        source: Source,
        options: PrepareOptions,
    ) -> Result<PreparedGraph, PrepareGraphError> {
        self.surface
            .prepare_graph_with_options(source, options)
            .await
    }

    /// Returns the permanent compatibility module-cache domain.
    #[must_use]
    pub fn default_module_domain(&mut self) -> ModuleDomainHandle {
        self.refresh_source_epoch();
        self.default_module_domain
    }

    /// Creates an independently releasable module-cache domain.
    pub fn create_module_domain(&mut self) -> ModuleDomainHandle {
        self.refresh_source_epoch();
        let raw_id = self.next_module_domain_id;
        self.next_module_domain_id = self
            .next_module_domain_id
            .checked_add(1)
            .expect("module-domain counter overflow");
        let id = ModuleDomainId::new(raw_id);
        let (slot, generation) = self.module_domains.insert(RetainedModuleDomain {
            id,
            permanent: false,
            roots: 0,
            invocations: 0,
        });
        ModuleDomainHandle::new(slot, generation, self.source_epoch, self.id, id)
    }

    /// Loads a prepared graph root and returns a generational handle.
    ///
    /// # Errors
    /// Returns [`LifecycleError::PreparedLoad`] if the graph is stale,
    /// mismatched, or cannot be loaded.
    pub fn load_prepared(
        &mut self,
        prepared: &PreparedGraph,
    ) -> Result<RootHandle, LifecycleError> {
        let domain = self.default_module_domain();
        self.load_prepared_in(domain, prepared)
    }

    /// Loads a prepared graph root in one module-cache domain.
    ///
    /// # Errors
    /// Returns a stale-domain error or [`LifecycleError::PreparedLoad`].
    pub fn load_prepared_in(
        &mut self,
        domain: ModuleDomainHandle,
        prepared: &PreparedGraph,
    ) -> Result<RootHandle, LifecycleError> {
        self.refresh_source_epoch();
        self.domain(domain)?;
        let module = prepared
            .load(&mut self.vm)
            .map_err(LifecycleError::PreparedLoad)?;
        self.vm
            .bind_chunk_environment(&module)
            .map_err(LifecycleError::BindEnvironment)?;
        let function = self.vm.stash_module_function(&module)?;
        Ok(self.insert_root(domain, module, function, prepared.runtime_compiler()))
    }

    /// Loads a compiled root with explicit requester/debug identity.
    ///
    /// # Errors
    /// Returns [`LifecycleError::Load`] when loading fails.
    pub fn load_compiled(
        &mut self,
        chunk: &BytecodeChunk,
        target: &LoadTarget,
    ) -> Result<RootHandle, LifecycleError> {
        let domain = self.default_module_domain();
        self.load_compiled_in(domain, chunk, target)
    }

    /// Loads a compiled root in one module-cache domain.
    ///
    /// # Errors
    /// Returns a stale-domain error or [`LifecycleError::Load`].
    pub fn load_compiled_in(
        &mut self,
        domain: ModuleDomainHandle,
        chunk: &BytecodeChunk,
        target: &LoadTarget,
    ) -> Result<RootHandle, LifecycleError> {
        self.refresh_source_epoch();
        self.domain(domain)?;
        let module = target
            .load(&mut self.vm, chunk)
            .map_err(LifecycleError::Load)?;
        self.vm
            .bind_chunk_environment(&module)
            .map_err(LifecycleError::BindEnvironment)?;
        let function = self.vm.stash_module_function(&module)?;
        Ok(self.insert_root(domain, module, function, None))
    }

    /// Runs a loaded root synchronously and returns owned values.
    ///
    /// # Errors
    /// Returns a stale-handle or VM execution error.
    pub fn run_ready(
        &mut self,
        root: &RootHandle,
        options: CallOptions,
    ) -> Result<Vec<ValueSnapshot>, LifecycleError> {
        self.refresh_source_epoch();
        let root_entry = Self::retained(
            self.id,
            self.source_epoch,
            &self.roots,
            root.key,
            HandleKind::Root,
        )?;
        let options = match &root_entry.runtime_compiler {
            Some(compiler) => options.runtime_compiler(Arc::clone(compiler)),
            None => options,
        };
        self.vm
            .exec_in_module_domain(root_entry.domain.id, &root_entry.module, options)
            .map_err(LifecycleError::Exec)
    }

    /// Runs a loaded root asynchronously and returns owned values.
    ///
    /// # Errors
    /// Returns a stale-handle or VM execution error.
    pub async fn run(
        &mut self,
        root: &RootHandle,
        options: CallOptions,
    ) -> Result<Vec<ValueSnapshot>, LifecycleError> {
        self.refresh_source_epoch();
        let root_entry = Self::retained(
            self.id,
            self.source_epoch,
            &self.roots,
            root.key,
            HandleKind::Root,
        )?;
        let options = match &root_entry.runtime_compiler {
            Some(compiler) => options.runtime_compiler(Arc::clone(compiler)),
            None => options,
        };
        self.vm
            .exec_async_in_module_domain(root_entry.domain.id, &root_entry.module, options)
            .await
            .map_err(LifecycleError::Exec)
    }

    /// Runs a loaded root asynchronously with a non-`Send` borrowed host context.
    ///
    /// # Errors
    /// Returns a stale-handle or VM execution error.
    pub async fn run_with_context<T: Any>(
        &mut self,
        root: &RootHandle,
        context: &mut T,
        options: CallOptions,
    ) -> Result<Vec<ValueSnapshot>, LifecycleError> {
        self.refresh_source_epoch();
        let root_entry = Self::retained(
            self.id,
            self.source_epoch,
            &self.roots,
            root.key,
            HandleKind::Root,
        )?;
        let options = match &root_entry.runtime_compiler {
            Some(compiler) => options.runtime_compiler(Arc::clone(compiler)),
            None => options,
        };
        self.vm
            .exec_async_with_context_in_module_domain(
                root_entry.domain.id,
                &root_entry.module,
                context,
                options,
            )
            .await
            .map_err(LifecycleError::Exec)
    }

    /// Creates a detached invocation for a loaded root.
    ///
    /// # Errors
    /// Returns a stale-root error or a VM setup error.
    pub fn create_root_invocation(
        &mut self,
        root: &RootHandle,
    ) -> Result<InvocationHandle, LifecycleError> {
        self.refresh_source_epoch();
        let root = Self::retained(
            self.id,
            self.source_epoch,
            &self.roots,
            root.key,
            HandleKind::Root,
        )?;
        let domain = root.domain;
        let runtime_compiler = root.runtime_compiler.clone();
        let invocation = self.vm.create_detached_root(&root.module)?;
        Ok(self.insert_invocation(domain, invocation, runtime_compiler))
    }

    /// Creates a detached invocation for a retained function.
    ///
    /// Each argument handle is consumed after the invocation has rooted its
    /// value. Duplicate, stale, or foreign argument handles fail without
    /// consuming any argument.
    ///
    /// # Errors
    /// Returns a stale function, domain, or value error, or a VM setup error.
    pub fn create_function_invocation(
        &mut self,
        domain: ModuleDomainHandle,
        function: &FunctionHandle,
        args: Vec<ValueHandle>,
    ) -> Result<InvocationHandle, LifecycleError> {
        self.refresh_source_epoch();
        self.domain(domain)?;
        let function = Arc::clone(&self.function(function)?.function);
        let mut unique = HashSet::with_capacity(args.len());
        let values = args
            .iter()
            .map(|handle| {
                if !unique.insert(*handle) {
                    return Err(Self::stale(
                        HandleKind::Value,
                        handle.key,
                        self.source_epoch,
                    ));
                }
                self.value(*handle).cloned()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let invocation = self.vm.create_detached_function(&function, &values)?;
        for handle in args {
            drop(Self::remove_retained(
                self.id,
                self.source_epoch,
                &mut self.values,
                handle.key,
                HandleKind::Value,
            )?);
        }
        Ok(self.insert_invocation(domain, invocation, None))
    }

    /// Polls one detached invocation segment.
    ///
    /// A ready result consumes the invocation. A pending result leaves it
    /// rooted in the runtime. `options` apply only while this poll executes VM
    /// work; they do not impose a deadline on parked host work. Use
    /// [`Runtime::abort_invocation`] as the invocation-lifetime bound.
    ///
    /// # Errors
    /// Returns a stale-invocation or VM execution error.
    pub fn poll_invocation(
        &mut self,
        handle: InvocationHandle,
        options: &CallOptions,
        context: &mut Context<'_>,
    ) -> Poll<Result<Vec<ValueSnapshot>, LifecycleError>> {
        let mut host_context = ();
        let step = self.poll_invocation_with_context_and_result(
            handle,
            &mut host_context,
            options,
            context,
            |scope, values| scope.marshal_values(values),
        );
        Self::owned_invocation_poll(step)
    }

    /// Polls one detached invocation segment with borrowed host context.
    ///
    /// The runtime lends `host_context` only for this poll and never retains the
    /// borrow while the invocation is pending.
    ///
    /// # Errors
    /// Returns a stale-invocation or VM execution error.
    pub fn poll_invocation_with_context<T: Any>(
        &mut self,
        handle: InvocationHandle,
        host_context: &mut T,
        options: &CallOptions,
        context: &mut Context<'_>,
    ) -> Poll<Result<Vec<ValueSnapshot>, LifecycleError>> {
        let step = self.poll_invocation_with_context_and_result(
            handle,
            host_context,
            options,
            context,
            |scope, values| scope.marshal_values(values),
        );
        Self::owned_invocation_poll(step)
    }

    /// Polls one detached invocation with borrowed host state and a scoped
    /// successful-completion callback.
    ///
    /// The callback runs once, while successful return values are still live.
    /// It may parse them or stash heap-backed values before the invocation is
    /// finalized. Pending polls do not call it.
    ///
    /// # Errors
    /// Returns a stale-invocation or VM execution error.
    pub fn poll_invocation_with_context_and_completion<T: Any>(
        &mut self,
        handle: InvocationHandle,
        host_context: &mut T,
        options: &CallOptions,
        context: &mut Context<'_>,
        completion: impl for<'s> FnOnce(&Scope<'s>, MultiValue<'s>) -> Result<(), RuntimeError>,
    ) -> Poll<Result<Vec<ValueSnapshot>, LifecycleError>> {
        let step = self.poll_invocation_with_context_and_result(
            handle,
            host_context,
            options,
            context,
            |scope, values| {
                completion(scope, values.clone())?;
                scope.marshal_values(values)
            },
        );
        Self::owned_invocation_poll(step)
    }

    /// Polls one detached invocation and decodes successful values in scope.
    ///
    /// The decoder runs exactly once for a successful ready result. It can
    /// inspect or stash heap-backed values. Pending polls do not call it. The
    /// returned usage belongs to this poll only.
    ///
    /// # Errors
    /// Returns [`InvocationError::Lifecycle`] for retained or VM failures.
    /// Returns [`InvocationError::Completion`] without changing the decoder's
    /// error type when successful-result decoding fails.
    pub fn poll_invocation_with_context_and_result<T: Any, R, E>(
        &mut self,
        handle: InvocationHandle,
        host_context: &mut T,
        options: &CallOptions,
        context: &mut Context<'_>,
        decode: impl for<'s> FnOnce(&Scope<'s>, MultiValue<'s>) -> Result<R, E>,
    ) -> InvocationStep<R, E> {
        self.refresh_source_epoch();
        let key = handle.key;
        let invocation = match Self::retained_mut(
            self.id,
            self.source_epoch,
            &mut self.invocations,
            key,
            HandleKind::Invocation,
        ) {
            Ok(invocation) => invocation,
            Err(error) => {
                return InvocationStep {
                    poll: Poll::Ready(Err(InvocationError::Lifecycle(error))),
                    usage: InvocationPollUsage::default(),
                };
            }
        };
        let step = with_optional_runtime_compiler(
            &mut self.vm,
            invocation.runtime_compiler.clone(),
            |vm| {
                vm.poll_detached_invocation(
                    &mut invocation.invocation,
                    invocation.domain.id,
                    host_context,
                    options,
                    context,
                    decode,
                )
            },
        );
        let poll = match step.poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => Poll::Ready(Ok(result)),
            Poll::Ready(Err(ruau_vm::DetachedInvocationError::Exec(error))) => {
                Poll::Ready(Err(InvocationError::Lifecycle(LifecycleError::Exec(error))))
            }
            Poll::Ready(Err(ruau_vm::DetachedInvocationError::Completion(error))) => {
                Poll::Ready(Err(InvocationError::Completion(error)))
            }
        };
        InvocationStep {
            poll: self.finish_invocation_poll(handle, poll),
            usage: InvocationPollUsage {
                gas_spent: step.gas_spent,
            },
        }
    }

    fn owned_invocation_poll(
        step: InvocationStep<Vec<ValueSnapshot>, RuntimeError>,
    ) -> Poll<Result<Vec<ValueSnapshot>, LifecycleError>> {
        match step.poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(values)) => Poll::Ready(Ok(values)),
            Poll::Ready(Err(InvocationError::Lifecycle(error))) => Poll::Ready(Err(error)),
            Poll::Ready(Err(InvocationError::Completion(error))) => {
                Poll::Ready(Err(LifecycleError::Exec(ExecError::Marshal {
                    message: error.message().to_owned(),
                })))
            }
        }
    }

    /// Aborts one detached invocation and consumes its handle.
    ///
    /// # Errors
    /// Returns a stale-handle error after completion, abort, invalidation, or
    /// use with a different runtime.
    pub fn abort_invocation(&mut self, handle: InvocationHandle) -> Result<(), LifecycleError> {
        self.refresh_source_epoch();
        let invocation = Self::remove_retained(
            self.id,
            self.source_epoch,
            &mut self.invocations,
            handle.key,
            HandleKind::Invocation,
        )?;
        self.vm.abort_detached_invocation(invocation.invocation);
        self.release_domain_invocation(invocation.domain);
        Ok(())
    }

    /// Opens a complete call-context scope step on the retained VM.
    ///
    /// Luau calls made by the body use the permanent default module-cache
    /// domain. Use [`step_root`](Self::step_root) when calls must inherit a
    /// loaded root's domain.
    ///
    /// # Errors
    /// Returns a VM scope failure.
    pub fn step<R: IntoStash>(
        &mut self,
        options: &CallOptions,
        body: impl for<'s> FnOnce(&Scope<'s>) -> Result<R, RuntimeError>,
    ) -> Result<R, LifecycleError> {
        self.refresh_source_epoch();
        self.vm
            .step_with(options, body)
            .map_err(LifecycleError::Runtime)
    }

    /// Opens a scope step with a loaded root's main function.
    ///
    /// This is the raw-result retained path: the body may call the root and
    /// stash returned tables, functions, or values before the scope closes.
    /// Luau calls made by the body use the loaded root's module-cache domain.
    ///
    /// # Errors
    /// Returns a stale-root or VM scope failure.
    pub fn step_root<R: IntoStash>(
        &mut self,
        root: &RootHandle,
        options: &CallOptions,
        body: impl for<'s> FnOnce(&Scope<'s>, Function<'s>) -> Result<R, RuntimeError>,
    ) -> Result<R, LifecycleError> {
        self.refresh_source_epoch();
        let root_entry = Self::retained(
            self.id,
            self.source_epoch,
            &self.roots,
            root.key,
            HandleKind::Root,
        )?;
        with_optional_runtime_compiler(&mut self.vm, root_entry.runtime_compiler.clone(), |vm| {
            vm.step_in_module_domain(root_entry.domain.id, options, |scope| {
                let main = scope.module_function(&root_entry.module);
                body(scope, main)
            })
        })
        .map_err(LifecycleError::Runtime)
    }

    /// Opens a scope step with a non-`Send` borrowed host context.
    ///
    /// Luau calls made by the body use the permanent default module-cache
    /// domain. Use
    /// [`step_root_with_context`](Self::step_root_with_context) when calls must
    /// inherit a loaded root's domain.
    ///
    /// # Errors
    /// Returns a VM scope failure.
    pub fn step_with_context<T: Any, R: IntoStash>(
        &mut self,
        context: &mut T,
        options: &CallOptions,
        body: impl for<'s> FnOnce(&Scope<'s>) -> Result<R, RuntimeError>,
    ) -> Result<R, LifecycleError> {
        self.refresh_source_epoch();
        self.vm
            .step_with_context(context, options, body)
            .map_err(LifecycleError::Runtime)
    }

    /// Opens a loaded-root scope step with a non-`Send` borrowed host context.
    ///
    /// # Errors
    /// Returns a stale-root or VM scope failure.
    pub fn step_root_with_context<T: Any, R: IntoStash>(
        &mut self,
        root: &RootHandle,
        context: &mut T,
        options: &CallOptions,
        body: impl for<'s> FnOnce(&Scope<'s>, Function<'s>) -> Result<R, RuntimeError>,
    ) -> Result<R, LifecycleError> {
        self.refresh_source_epoch();
        let root_entry = Self::retained(
            self.id,
            self.source_epoch,
            &self.roots,
            root.key,
            HandleKind::Root,
        )?;
        with_optional_runtime_compiler(&mut self.vm, root_entry.runtime_compiler.clone(), |vm| {
            vm.step_with_context_in_module_domain(root_entry.domain.id, context, options, |scope| {
                let main = scope.module_function(&root_entry.module);
                body(scope, main)
            })
        })
        .map_err(LifecycleError::Runtime)
    }

    /// Moves a typed VM stash into this runtime's generational arena.
    pub fn retain<S: Retain>(&mut self, stash: S) -> S::Handle {
        stash.retain(self)
    }

    /// Opens a safe scope over a retained typed handle.
    ///
    /// Luau calls made by the body use the permanent default module-cache
    /// domain. Retained value, table, and function handles do not preserve the
    /// domain in which they were created.
    ///
    /// # Errors
    /// Returns a stale-handle or VM scope failure.
    pub fn get<H, R, F>(
        &mut self,
        handle: H,
        options: &CallOptions,
        body: F,
    ) -> Result<R, LifecycleError>
    where
        H: Handle,
        R: IntoStash,
        F: for<'scope> FnOnce(&Scope<'scope>, H::Scoped<'scope>) -> Result<R, RuntimeError>,
    {
        handle.get(self, options, body)
    }

    /// Releases a retained typed handle.
    ///
    /// # Errors
    /// Returns a stale-handle error if it was already released or invalidated.
    pub fn release<H: Handle>(&mut self, handle: H) -> Result<(), LifecycleError> {
        handle.release(self)
    }

    fn retain_value(&mut self, stash: StashedValue) -> ValueHandle {
        self.refresh_source_epoch();
        let (slot, generation) = self.values.insert(stash);
        ValueHandle::new(self.id, slot, generation, self.source_epoch)
    }

    /// Moves a table stash into this runtime's generational arena.
    fn retain_table(&mut self, stash: StashedTable) -> TableHandle {
        self.refresh_source_epoch();
        let (slot, generation) = self.tables.insert(stash);
        TableHandle::new(self.id, slot, generation, self.source_epoch)
    }

    /// Moves a function stash into this runtime's generational arena.
    fn retain_function(&mut self, stash: StashedClosure) -> FunctionHandle {
        self.refresh_source_epoch();
        let function = Arc::new(stash);
        let (slot, generation) = self.functions.insert(RetainedFunction {
            function: Arc::clone(&function),
        });
        FunctionHandle::new(
            self.id,
            slot,
            generation,
            self.source_epoch,
            Arc::downgrade(&function),
            Arc::clone(&self.current_epoch),
        )
    }

    /// Fetches a generic value inside one safe scope step.
    ///
    /// # Errors
    /// Returns a stale-handle or VM scope failure.
    fn get_value<R: IntoStash>(
        &mut self,
        handle: ValueHandle,
        options: &CallOptions,
        body: impl for<'s> FnOnce(&Scope<'s>, ScopedValue<'s>) -> Result<R, RuntimeError>,
    ) -> Result<R, LifecycleError> {
        self.refresh_source_epoch();
        let stash = self.value(handle)?.clone();
        self.vm
            .step_with(options, |scope| {
                let value = scope.fetch_value(&stash)?;
                body(scope, value)
            })
            .map_err(LifecycleError::Runtime)
    }

    /// Fetches a retained table inside one safe scope step.
    ///
    /// # Errors
    /// Returns a stale-handle or VM scope failure.
    fn get_table<R: IntoStash>(
        &mut self,
        handle: TableHandle,
        options: &CallOptions,
        body: impl for<'s> FnOnce(&Scope<'s>, Table<'s>) -> Result<R, RuntimeError>,
    ) -> Result<R, LifecycleError> {
        self.refresh_source_epoch();
        let stash = self.table(handle)?.clone();
        self.vm
            .step_with(options, |scope| {
                let table = scope.fetch_table(&stash)?;
                body(scope, table)
            })
            .map_err(LifecycleError::Runtime)
    }

    /// Fetches a retained function inside one safe scope step.
    ///
    /// # Errors
    /// Returns a stale-handle or VM scope failure.
    fn get_function<R: IntoStash>(
        &mut self,
        handle: &FunctionHandle,
        options: &CallOptions,
        body: impl for<'s> FnOnce(&Scope<'s>, Function<'s>) -> Result<R, RuntimeError>,
    ) -> Result<R, LifecycleError> {
        self.refresh_source_epoch();
        let stash = Arc::clone(&self.function(handle)?.function);
        self.vm
            .step_with(options, |scope| {
                let function = scope.fetch_function(&stash)?;
                body(scope, function)
            })
            .map_err(LifecycleError::Runtime)
    }

    /// Explicitly releases a generic value handle.
    ///
    /// # Errors
    /// Returns a stale-handle error if it was already released or invalidated.
    fn release_value(&mut self, handle: ValueHandle) -> Result<(), LifecycleError> {
        self.refresh_source_epoch();
        self.remove_value(handle).map(drop)
    }

    /// Explicitly releases a table handle.
    ///
    /// # Errors
    /// Returns a stale-handle error if it was already released or invalidated.
    fn release_table(&mut self, handle: TableHandle) -> Result<(), LifecycleError> {
        self.refresh_source_epoch();
        self.remove_table(handle).map(drop)
    }

    /// Explicitly releases a function handle.
    ///
    /// # Errors
    /// Returns a stale-handle error if it was already released or invalidated.
    fn release_function(&mut self, handle: &FunctionHandle) -> Result<(), LifecycleError> {
        self.refresh_source_epoch();
        self.remove_function(handle).map(drop)
    }

    /// Unloads a retained root and invalidates its handle.
    ///
    /// # Errors
    /// Returns a stale-handle error if it was already unloaded or invalidated.
    pub fn unload(&mut self, handle: &RootHandle) -> Result<(), LifecycleError> {
        self.refresh_source_epoch();
        let root = self.remove_root(handle)?;
        self.release_domain_root(root.domain);
        self.vm.unload(root.module);
        Ok(())
    }

    /// Releases one unused module-cache domain.
    ///
    /// # Errors
    /// Returns a stale-handle error, [`LifecycleError::PermanentHandle`] for the
    /// default domain, or [`LifecycleError::InUse`] while a root or invocation
    /// still uses the domain.
    pub fn release_module_domain(
        &mut self,
        handle: ModuleDomainHandle,
    ) -> Result<ModuleDomainRelease, LifecycleError> {
        self.refresh_source_epoch();
        let domain = self.domain(handle)?;
        if domain.permanent {
            return Err(LifecycleError::PermanentHandle {
                kind: HandleKind::ModuleDomain,
            });
        }
        if domain.roots != 0 || domain.invocations != 0 {
            return Err(LifecycleError::InUse {
                kind: HandleKind::ModuleDomain,
            });
        }
        let domain =
            Self::remove_domain(self.id, self.source_epoch, &mut self.module_domains, handle)?;
        let (cached_modules, in_flight_modules) = self.vm.clear_module_cache_domain(domain.id);
        Ok(ModuleDomainRelease {
            cached_modules,
            in_flight_modules,
        })
    }

    /// Releases every retained root and stash, clears the module cache, and
    /// advances to the source's current epoch.
    pub fn invalidate(&mut self) -> Invalidation {
        let current_epoch = surface_source_epoch(&self.surface);
        self.invalidate_to(current_epoch)
    }

    /// Invalidates retained state only when the source epoch changed.
    pub fn invalidate_if_source_changed(&mut self) -> Option<Invalidation> {
        let current_epoch = surface_source_epoch(&self.surface);
        (current_epoch != self.source_epoch).then(|| self.invalidate_to(current_epoch))
    }

    fn refresh_source_epoch(&mut self) {
        let _ = self.invalidate_if_source_changed();
    }

    fn invalidate_to(&mut self, current_epoch: u64) -> Invalidation {
        let previous_epoch = self.source_epoch;
        self.current_epoch.store(current_epoch, Ordering::Release);
        let invocations = self.invocations.drain();
        let invocation_count = invocations.len();
        for invocation in invocations {
            self.vm.abort_detached_invocation(invocation.invocation);
        }
        let roots = self.roots.drain();
        let root_count = roots.len();
        for root in roots {
            self.vm.unload(root.module);
        }
        let value_count = self.values.clear();
        let table_count = self.tables.clear();
        let functions = self.functions.drain();
        let function_count = functions.len();
        drop(functions);
        self.vm.clear_module_cache();
        // Drain release notifications from dropped stashes without counting an
        // execution. A poisoned VM cannot safely enter, so its heap is left for
        // drop instead.
        drop(self.vm.step(|_| Ok(())));
        self.source_epoch = current_epoch;
        let module_domain_count = self.module_domains.clear();
        self.default_module_domain = self.insert_default_module_domain();
        Invalidation {
            previous_epoch,
            current_epoch,
            roots: root_count,
            values: value_count,
            tables: table_count,
            functions: function_count,
            module_domains: module_domain_count,
            invocations: invocation_count,
        }
    }

    fn insert_root(
        &mut self,
        domain: ModuleDomainHandle,
        module: LoadedModule,
        function: StashedClosure,
        runtime_compiler: Option<Arc<dyn RuntimeCompiler>>,
    ) -> RootHandle {
        self.domain_mut(domain)
            .expect("validated module domain remains live")
            .roots += 1;
        let function = Arc::new(function);
        let (slot, generation) = self.roots.insert(RetainedRoot {
            module,
            _function: Arc::clone(&function),
            domain,
            runtime_compiler,
        });
        RootHandle::new(
            self.id,
            slot,
            generation,
            self.source_epoch,
            Arc::downgrade(&function),
            Arc::clone(&self.current_epoch),
        )
    }

    fn insert_invocation(
        &mut self,
        domain: ModuleDomainHandle,
        invocation: ruau_vm::DetachedInvocation,
        runtime_compiler: Option<Arc<dyn RuntimeCompiler>>,
    ) -> InvocationHandle {
        self.domain_mut(domain)
            .expect("validated module domain remains live")
            .invocations += 1;
        let (slot, generation) = self.invocations.insert(RetainedInvocation {
            invocation,
            domain,
            runtime_compiler,
        });
        InvocationHandle::new(self.id, slot, generation, self.source_epoch)
    }

    fn finish_invocation_poll<R, E>(
        &mut self,
        handle: InvocationHandle,
        poll: Poll<Result<R, E>>,
    ) -> Poll<Result<R, E>> {
        match poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                let invocation = Self::remove_retained(
                    self.id,
                    self.source_epoch,
                    &mut self.invocations,
                    handle.key,
                    HandleKind::Invocation,
                )
                .expect("a ready invocation remains live until result consumption");
                self.release_domain_invocation(invocation.domain);
                Poll::Ready(result)
            }
        }
    }

    fn insert_default_module_domain(&mut self) -> ModuleDomainHandle {
        let (slot, generation) = self.module_domains.insert(RetainedModuleDomain {
            id: ModuleDomainId::DEFAULT,
            permanent: true,
            roots: 0,
            invocations: 0,
        });
        ModuleDomainHandle::new(
            slot,
            generation,
            self.source_epoch,
            self.id,
            ModuleDomainId::DEFAULT,
        )
    }

    fn domain(&self, handle: ModuleDomainHandle) -> Result<&RetainedModuleDomain, LifecycleError> {
        if handle.key.owner == self.id {
            return Self::retained(
                self.id,
                self.source_epoch,
                &self.module_domains,
                handle.key,
                HandleKind::ModuleDomain,
            );
        }
        Err(Self::stale(
            HandleKind::ModuleDomain,
            handle.key,
            self.source_epoch,
        ))
    }

    fn domain_mut(
        &mut self,
        handle: ModuleDomainHandle,
    ) -> Result<&mut RetainedModuleDomain, LifecycleError> {
        if handle.key.owner == self.id
            && handle.key.source_epoch == self.source_epoch
            && let Some(domain) = self
                .module_domains
                .get_mut(handle.key.slot, handle.key.generation)
        {
            return Ok(domain);
        }
        Err(Self::stale(
            HandleKind::ModuleDomain,
            handle.key,
            self.source_epoch,
        ))
    }

    fn remove_domain(
        owner: u64,
        current_epoch: u64,
        domains: &mut GenerationalArena<RetainedModuleDomain>,
        handle: ModuleDomainHandle,
    ) -> Result<RetainedModuleDomain, LifecycleError> {
        if handle.key.owner == owner {
            return Self::remove_retained(
                owner,
                current_epoch,
                domains,
                handle.key,
                HandleKind::ModuleDomain,
            );
        }
        Err(Self::stale(
            HandleKind::ModuleDomain,
            handle.key,
            current_epoch,
        ))
    }

    fn release_domain_root(&mut self, handle: ModuleDomainHandle) {
        let domain = self
            .domain_mut(handle)
            .expect("a retained root keeps its module domain live");
        domain.roots = domain
            .roots
            .checked_sub(1)
            .expect("module-domain root count underflow");
    }

    fn release_domain_invocation(&mut self, handle: ModuleDomainHandle) {
        let domain = self
            .domain_mut(handle)
            .expect("a retained invocation keeps its module domain live");
        domain.invocations = domain
            .invocations
            .checked_sub(1)
            .expect("module-domain invocation count underflow");
    }

    fn value(&self, handle: ValueHandle) -> Result<&StashedValue, LifecycleError> {
        Self::retained(
            self.id,
            self.source_epoch,
            &self.values,
            handle.key,
            HandleKind::Value,
        )
    }

    fn table(&self, handle: TableHandle) -> Result<&StashedTable, LifecycleError> {
        Self::retained(
            self.id,
            self.source_epoch,
            &self.tables,
            handle.key,
            HandleKind::Table,
        )
    }

    fn function(&self, handle: &FunctionHandle) -> Result<&RetainedFunction, LifecycleError> {
        Self::retained(
            self.id,
            self.source_epoch,
            &self.functions,
            handle.key,
            HandleKind::Function,
        )
    }

    fn remove_root(&mut self, handle: &RootHandle) -> Result<RetainedRoot, LifecycleError> {
        Self::remove_retained(
            self.id,
            self.source_epoch,
            &mut self.roots,
            handle.key,
            HandleKind::Root,
        )
    }

    fn remove_value(&mut self, handle: ValueHandle) -> Result<StashedValue, LifecycleError> {
        Self::remove_retained(
            self.id,
            self.source_epoch,
            &mut self.values,
            handle.key,
            HandleKind::Value,
        )
    }

    fn remove_table(&mut self, handle: TableHandle) -> Result<StashedTable, LifecycleError> {
        Self::remove_retained(
            self.id,
            self.source_epoch,
            &mut self.tables,
            handle.key,
            HandleKind::Table,
        )
    }

    fn remove_function(
        &mut self,
        handle: &FunctionHandle,
    ) -> Result<RetainedFunction, LifecycleError> {
        Self::remove_retained(
            self.id,
            self.source_epoch,
            &mut self.functions,
            handle.key,
            HandleKind::Function,
        )
    }

    fn retained<T>(
        current_owner: u64,
        current_epoch: u64,
        arena: &GenerationalArena<T>,
        key: HandleKey,
        kind: HandleKind,
    ) -> Result<&T, LifecycleError> {
        if key.owner == current_owner
            && key.source_epoch == current_epoch
            && let Some(value) = arena.get(key.slot, key.generation)
        {
            return Ok(value);
        }
        Err(Self::stale(kind, key, current_epoch))
    }

    fn retained_mut<T>(
        current_owner: u64,
        current_epoch: u64,
        arena: &mut GenerationalArena<T>,
        key: HandleKey,
        kind: HandleKind,
    ) -> Result<&mut T, LifecycleError> {
        if key.owner == current_owner
            && key.source_epoch == current_epoch
            && let Some(value) = arena.get_mut(key.slot, key.generation)
        {
            return Ok(value);
        }
        Err(Self::stale(kind, key, current_epoch))
    }

    fn remove_retained<T>(
        current_owner: u64,
        current_epoch: u64,
        arena: &mut GenerationalArena<T>,
        key: HandleKey,
        kind: HandleKind,
    ) -> Result<T, LifecycleError> {
        if key.owner == current_owner
            && key.source_epoch == current_epoch
            && let Some(value) = arena.remove(key.slot, key.generation)
        {
            return Ok(value);
        }
        Err(Self::stale(kind, key, current_epoch))
    }

    fn stale(kind: HandleKind, key: HandleKey, current_epoch: u64) -> LifecycleError {
        LifecycleError::StaleHandle {
            kind,
            handle_epoch: key.source_epoch,
            current_epoch,
        }
    }
}

impl handle_sealed::RetainSealed for StashedValue {}
impl Retain for StashedValue {
    type Handle = ValueHandle;

    fn retain(self, runtime: &mut Runtime) -> Self::Handle {
        runtime.retain_value(self)
    }
}

impl handle_sealed::RetainSealed for StashedTable {}
impl Retain for StashedTable {
    type Handle = TableHandle;

    fn retain(self, runtime: &mut Runtime) -> Self::Handle {
        runtime.retain_table(self)
    }
}

impl handle_sealed::RetainSealed for StashedClosure {}
impl Retain for StashedClosure {
    type Handle = FunctionHandle;

    fn retain(self, runtime: &mut Runtime) -> Self::Handle {
        runtime.retain_function(self)
    }
}

impl handle_sealed::Sealed for ValueHandle {}
impl Handle for ValueHandle {
    type Scoped<'scope> = ScopedValue<'scope>;

    fn get<R, F>(
        self,
        runtime: &mut Runtime,
        options: &CallOptions,
        body: F,
    ) -> Result<R, LifecycleError>
    where
        R: IntoStash,
        F: for<'scope> FnOnce(&Scope<'scope>, Self::Scoped<'scope>) -> Result<R, RuntimeError>,
    {
        runtime.get_value(self, options, body)
    }

    fn release(self, runtime: &mut Runtime) -> Result<(), LifecycleError> {
        runtime.release_value(self)
    }
}

impl handle_sealed::Sealed for TableHandle {}
impl Handle for TableHandle {
    type Scoped<'scope> = Table<'scope>;

    fn get<R, F>(
        self,
        runtime: &mut Runtime,
        options: &CallOptions,
        body: F,
    ) -> Result<R, LifecycleError>
    where
        R: IntoStash,
        F: for<'scope> FnOnce(&Scope<'scope>, Self::Scoped<'scope>) -> Result<R, RuntimeError>,
    {
        runtime.get_table(self, options, body)
    }

    fn release(self, runtime: &mut Runtime) -> Result<(), LifecycleError> {
        runtime.release_table(self)
    }
}

impl handle_sealed::Sealed for &FunctionHandle {}
impl Handle for &FunctionHandle {
    type Scoped<'scope> = Function<'scope>;

    fn get<R, F>(
        self,
        runtime: &mut Runtime,
        options: &CallOptions,
        body: F,
    ) -> Result<R, LifecycleError>
    where
        R: IntoStash,
        F: for<'scope> FnOnce(&Scope<'scope>, Self::Scoped<'scope>) -> Result<R, RuntimeError>,
    {
        runtime.get_function(self, options, body)
    }

    fn release(self, runtime: &mut Runtime) -> Result<(), LifecycleError> {
        runtime.release_function(self)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.current_epoch
            .store(self.source_epoch.wrapping_add(1), Ordering::Release);
        for invocation in self.invocations.drain() {
            self.vm.abort_detached_invocation(invocation.invocation);
        }
        for root in self.roots.drain() {
            self.vm.unload(root.module);
        }
        drop(self.functions.drain());
    }
}

fn surface_source_epoch(surface: &Surface) -> u64 {
    surface.module_source().map_or(0, |source| source.epoch())
}

struct ArenaSlot<T> {
    generation: u64,
    value: Option<T>,
}

struct GenerationalArena<T> {
    slots: Vec<ArenaSlot<T>>,
    free: Vec<usize>,
}

impl<T> Default for GenerationalArena<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}

impl<T> GenerationalArena<T> {
    fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    fn insert(&mut self, value: T) -> (usize, u64) {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index];
            debug_assert!(slot.value.is_none());
            slot.value = Some(value);
            return (index, slot.generation);
        }
        let index = self.slots.len();
        self.slots.push(ArenaSlot {
            generation: 1,
            value: Some(value),
        });
        (index, 1)
    }

    fn get(&self, index: usize, generation: u64) -> Option<&T> {
        let slot = self.slots.get(index)?;
        (slot.generation == generation)
            .then_some(slot.value.as_ref())
            .flatten()
    }

    fn get_mut(&mut self, index: usize, generation: u64) -> Option<&mut T> {
        let slot = self.slots.get_mut(index)?;
        (slot.generation == generation)
            .then_some(slot.value.as_mut())
            .flatten()
    }

    fn remove(&mut self, index: usize, generation: u64) -> Option<T> {
        let slot = self.slots.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        let value = slot.value.take()?;
        slot.generation = next_generation(slot.generation);
        self.free.push(index);
        Some(value)
    }

    fn drain(&mut self) -> Vec<T> {
        let mut values = Vec::new();
        self.free.clear();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if let Some(value) = slot.value.take() {
                values.push(value);
                slot.generation = next_generation(slot.generation);
            }
            self.free.push(index);
        }
        values
    }

    fn clear(&mut self) -> usize {
        self.drain().len()
    }
}

fn next_generation(generation: u64) -> u64 {
    generation.wrapping_add(1).max(1)
}

#[cfg(any())]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use ruau_declaration::DeclarationSource;
    use ruau_source::{SourceError, SourceMetadata, SourceResult, SyncSourceProvider};
    use ruau_vm::{
        IntoLuaMulti, Limits, ModuleBinding, MultiValue, NativeModule, ScopedHostFunction,
        module::{Installer as ModuleBuilder, InstallerExt as ModuleBuilderExt},
    };

    use super::*;

    fn runtime(surface: Surface) -> Runtime {
        Runtime::new(
            surface,
            &VmConfig::untrusted(ruau_vm::Ambient::deterministic(0), Limits::unlimited()),
        )
        .expect("runtime builds")
    }

    fn compile(runtime: &Runtime, source: &str) -> BytecodeChunk {
        runtime
            .surface()
            .compile(
                &Source::text(ModuleId::canonicalized("test"), source),
                &ruau_bytecode::CompileOptions::default(),
            )
            .expect("source compiles")
    }

    fn number(values: &[ValueSnapshot]) -> i64 {
        match values {
            [ValueSnapshot::Integer(value)] => *value,
            [ValueSnapshot::Number(value)] => *value as i64,
            other => panic!("expected one number, got {other:?}"),
        }
    }

    struct ContextCounter(u32);

    struct ContextModule;

    impl NativeModule for ContextModule {
        fn name(&self) -> &str {
            "retained_context"
        }

        fn declaration(&self) -> DeclarationSource<'_> {
            DeclarationSource::Text("declare read_context_counter: () -> number")
        }

        fn install(&self, builder: &mut dyn ModuleBuilder) {
            builder.scoped_function(
                "read_context_counter",
                ModuleBinding::Global,
                Box::new(ReadContextCounter),
            );
        }
    }

    struct ReadContextCounter;

    impl ScopedHostFunction for ReadContextCounter {
        fn call<'s>(
            &self,
            scope: &Scope<'s>,
            _args: MultiValue<'s>,
        ) -> Result<MultiValue<'s>, RuntimeError> {
            let mut context = scope
                .context_mut::<ContextCounter>()
                .ok_or_else(|| RuntimeError::runtime("missing borrowed context"))?;
            context.0 += 1;
            i64::from(context.0).into_lua_multi(scope)
        }
    }

    struct AttemptModule {
        attempts: Arc<AtomicU64>,
    }

    impl NativeModule for AttemptModule {
        fn name(&self) -> &str {
            "attempt"
        }

        fn declaration(&self) -> DeclarationSource<'_> {
            DeclarationSource::Text("declare next_module_attempt: () -> number")
        }

        fn install(&self, builder: &mut dyn ModuleBuilder) {
            builder.scoped_function(
                "next_module_attempt",
                ModuleBinding::Global,
                Box::new(NextModuleAttempt {
                    attempts: Arc::clone(&self.attempts),
                }),
            );
        }
    }

    struct NextModuleAttempt {
        attempts: Arc<AtomicU64>,
    }

    impl ScopedHostFunction for NextModuleAttempt {
        fn call<'s>(
            &self,
            scope: &Scope<'s>,
            _args: MultiValue<'s>,
        ) -> Result<MultiValue<'s>, RuntimeError> {
            let attempt = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;
            i64::try_from(attempt)
                .expect("test attempt count fits i64")
                .into_lua_multi(scope)
        }
    }

    struct DetachedModule {
        pause_gate: Arc<tokio::sync::Notify>,
    }

    impl NativeModule for DetachedModule {
        fn name(&self) -> &str {
            "detached"
        }

        fn declaration(&self) -> DeclarationSource<'_> {
            DeclarationSource::Text(
                "declare pause: (number) -> number\n\
                 declare read_detached_context: () -> number\n\
                 declare call_detached_callback: (number) -> number\n\
                 declare panic_detached: () -> never",
            )
        }

        fn install(&self, builder: &mut dyn ModuleBuilder) {
            let pause_gate = Arc::clone(&self.pause_gate);
            builder.async_function_fn(
                "pause",
                ModuleBinding::Global,
                move |_context, value: f64| {
                    let pause_gate = Arc::clone(&pause_gate);
                    async move {
                        pause_gate.notified().await;
                        Ok(ruau_vm::HostReturn {
                            values: vec![ruau_vm::OwnedValue::Number(value)],
                        })
                    }
                },
            );
            builder.async_function_fn(
                "read_detached_context",
                ModuleBinding::Global,
                |context, (): ()| async move {
                    tokio::task::yield_now().await;
                    let value = context
                        .scope(|scope| {
                            let mut context =
                                scope.context_mut::<ContextCounter>().ok_or_else(|| {
                                    RuntimeError::runtime("missing detached host context")
                                })?;
                            context.0 += 1;
                            Ok(i64::from(context.0))
                        })
                        .await?;
                    Ok(ruau_vm::HostReturn {
                        values: vec![ruau_vm::OwnedValue::Integer(value)],
                    })
                },
            );
            builder.async_function_fn(
                "call_detached_callback",
                ModuleBinding::Global,
                |context, value: f64| async move {
                    let callback = context
                        .scope(|scope| {
                            let Some(callback) = scope.global_function(b"detached_callback") else {
                                return Err(RuntimeError::runtime(
                                    "detached_callback is not a function",
                                ));
                            };
                            scope.stash_function(callback)
                        })
                        .await?;
                    context
                        .call_protected(&callback, (value,))
                        .await?
                        .map_err(|error| RuntimeError::runtime(error.message()))
                },
            );
            builder.async_function_fn(
                "panic_detached",
                ModuleBinding::Global,
                |_context, (): ()| async move {
                    panic!("detached host panic");
                    #[allow(unreachable_code)]
                    Ok(ruau_vm::HostReturn { values: Vec::new() })
                },
            );
        }
    }

    fn poll_once(
        runtime: &mut Runtime,
        invocation: InvocationHandle,
    ) -> Poll<Result<Vec<ValueSnapshot>, LifecycleError>> {
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        runtime.poll_invocation(invocation, &CallOptions::new(), &mut context)
    }

    fn poll_result_once<R, E>(
        runtime: &mut Runtime,
        invocation: InvocationHandle,
        options: &CallOptions,
        decode: impl for<'s> FnOnce(&Scope<'s>, MultiValue<'s>) -> Result<R, E>,
    ) -> InvocationStep<R, E> {
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        runtime.poll_invocation_with_context_and_result(
            invocation,
            &mut (),
            options,
            &mut context,
            decode,
        )
    }

    #[tokio::test]
    async fn typed_detached_poll_decodes_cyclic_result_and_reports_matching_gas() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let domain = runtime.create_module_domain();
        let chunk = compile(
            &runtime,
            "pause(1)\nlocal result = {}\nresult.self = result\nreturn result",
        );
        let root = runtime
            .load_compiled_in(
                domain,
                &chunk,
                &LoadTarget::named("typed-cyclic-result.luau"),
            )
            .expect("root loads");
        let invocation = runtime
            .create_root_invocation(&root)
            .expect("invocation starts");
        let options = CallOptions::new().limits(Limits {
            gas: Some(1_000_000),
            ..Limits::unlimited()
        });

        let pending = poll_result_once(
            &mut runtime,
            invocation,
            &options,
            |_scope, _values| -> Result<(), RuntimeError> {
                panic!("pending poll must not decode")
            },
        );
        assert!(pending.poll.is_pending());
        assert!(pending.usage.gas_spent > 0);
        assert_eq!(pending.usage.gas_spent, runtime.gas_spent());

        pause_gate.notify_one();
        tokio::task::yield_now().await;
        let ready = poll_result_once(&mut runtime, invocation, &options, |scope, values| {
            let values = values.into_vec();
            let [ScopedValue::Table(table)] = values.as_slice() else {
                return Err(RuntimeError::external("expected one table result"));
            };
            let nested: Table<'_> = table.get(scope, "self")?;
            if nested.id() != table.id() {
                return Err(RuntimeError::external("table cycle lost identity"));
            }
            Ok(table.id())
        });
        assert!(ready.usage.gas_spent > 0);
        assert_eq!(ready.usage.gas_spent, runtime.gas_spent());
        assert!(matches!(ready.poll, Poll::Ready(Ok(_))));

        let stale = poll_result_once(&mut runtime, invocation, &options, |_scope, _values| {
            Ok::<(), RuntimeError>(())
        });
        assert_eq!(stale.usage.gas_spent, 0);
        assert!(matches!(
            stale.poll,
            Poll::Ready(Err(InvocationError::Lifecycle(
                LifecycleError::StaleHandle {
                    kind: HandleKind::Invocation,
                    ..
                }
            )))
        ));

        runtime.unload(&root).expect("root unloads");
        runtime
            .release_module_domain(domain)
            .expect("completed invocation releases its domain use");
    }

    #[derive(Debug, Eq, PartialEq)]
    struct DecodeError(&'static str);

    impl fmt::Display for DecodeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl StdError for DecodeError {}

    #[test]
    fn typed_detached_decoder_error_is_preserved_and_finalized() {
        let mut runtime = runtime(Surface::new());
        let domain = runtime.create_module_domain();
        let chunk = compile(&runtime, "return 42");
        let root = runtime
            .load_compiled_in(
                domain,
                &chunk,
                &LoadTarget::named("typed-decoder-error.luau"),
            )
            .expect("root loads");
        let invocation = runtime
            .create_root_invocation(&root)
            .expect("invocation starts");
        let options = CallOptions::new().limits(Limits {
            gas: Some(1_000_000),
            ..Limits::unlimited()
        });

        let step = poll_result_once(&mut runtime, invocation, &options, |_scope, _values| {
            Err::<(), _>(DecodeError("invalid host result"))
        });
        assert!(step.usage.gas_spent > 0);
        let Poll::Ready(Err(error @ InvocationError::Completion(DecodeError(message)))) = step.poll
        else {
            panic!("decoder error should be preserved");
        };
        assert_eq!(message, "invalid host result");
        assert_eq!(
            error.to_string(),
            "retained completion failed: invalid host result"
        );
        assert!(StdError::source(&error).is_some());

        let stale = poll_result_once(&mut runtime, invocation, &options, |_scope, _values| {
            Ok::<(), DecodeError>(())
        });
        assert_eq!(stale.usage.gas_spent, 0);
        assert!(matches!(
            stale.poll,
            Poll::Ready(Err(InvocationError::Lifecycle(
                LifecycleError::StaleHandle { .. }
            )))
        ));
        runtime.unload(&root).expect("root unloads");
        runtime
            .release_module_domain(domain)
            .expect("decoder failure releases its domain use");
    }

    #[test]
    fn typed_detached_script_and_deadline_failures_finalize() {
        let mut runtime = runtime(Surface::new());
        let options = CallOptions::new().limits(Limits {
            gas: Some(1_000_000),
            ..Limits::unlimited()
        });
        let script_chunk = compile(&runtime, "error('typed boom')");
        let script_root = runtime
            .load_compiled(&script_chunk, &LoadTarget::named("typed-script-error.luau"))
            .expect("script-error root loads");
        let script_invocation = runtime
            .create_root_invocation(&script_root)
            .expect("script-error invocation starts");
        let mut decoded = false;
        let script_step = poll_result_once(
            &mut runtime,
            script_invocation,
            &options,
            |_scope, _values| {
                decoded = true;
                Ok::<(), DecodeError>(())
            },
        );
        assert!(!decoded);
        assert!(script_step.usage.gas_spent > 0);
        assert!(matches!(
            script_step.poll,
            Poll::Ready(Err(InvocationError::Lifecycle(LifecycleError::Exec(
                ExecError::Script(_)
            ))))
        ));

        let deadline_chunk = compile(&runtime, "while true do end");
        let deadline_root = runtime
            .load_compiled(&deadline_chunk, &LoadTarget::named("typed-deadline.luau"))
            .expect("deadline root loads");
        let deadline_invocation = runtime
            .create_root_invocation(&deadline_root)
            .expect("deadline invocation starts");
        let expired = CallOptions::new().limits(Limits {
            deadline: Some(ruau_vm::Deadline::Logical(100)),
            ..Limits::unlimited()
        });
        let deadline_step = poll_result_once(
            &mut runtime,
            deadline_invocation,
            &expired,
            |_scope, _values| Ok::<(), DecodeError>(()),
        );
        assert!(deadline_step.usage.gas_spent > 0);
        assert!(matches!(
            deadline_step.poll,
            Poll::Ready(Err(InvocationError::Lifecycle(LifecycleError::Exec(
                ExecError::Stopped(ruau_vm::StopReason::Deadline)
            ))))
        ));
    }

    #[tokio::test]
    async fn typed_detached_driver_panic_reports_segment_gas() {
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::new(tokio::sync::Notify::new()),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(&runtime, "return panic_detached()");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("typed-driver-panic.luau"))
            .expect("root loads");
        let invocation = runtime
            .create_root_invocation(&root)
            .expect("invocation starts");
        let options = CallOptions::new().limits(Limits {
            gas: Some(1_000_000),
            ..Limits::unlimited()
        });

        for _ in 0..10 {
            let step = poll_result_once(&mut runtime, invocation, &options, |_scope, _values| {
                Ok::<(), DecodeError>(())
            });
            match step.poll {
                Poll::Pending => tokio::task::yield_now().await,
                Poll::Ready(Err(InvocationError::Lifecycle(LifecycleError::Exec(
                    ExecError::PanicPoison,
                )))) => {
                    assert!(step.usage.gas_spent > 0);
                    return;
                }
                other => panic!("unexpected driver-panic result: {other:?}"),
            }
        }
        panic!("driver panic did not complete");
    }

    #[test]
    fn typed_detached_decoder_panic_poison_is_a_lifecycle_error() {
        let mut runtime = runtime(Surface::new());
        let chunk = compile(&runtime, "return 42");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("typed-decoder-panic.luau"))
            .expect("root loads");
        let invocation = runtime
            .create_root_invocation(&root)
            .expect("invocation starts");
        let options = CallOptions::new().limits(Limits {
            gas: Some(1_000_000),
            ..Limits::unlimited()
        });

        let step = poll_result_once(
            &mut runtime,
            invocation,
            &options,
            |_scope, _values| -> Result<(), DecodeError> { panic!("decoder panic") },
        );
        assert!(step.usage.gas_spent > 0);
        assert!(matches!(
            step.poll,
            Poll::Ready(Err(InvocationError::Lifecycle(LifecycleError::Exec(
                ExecError::PanicPoison
            ))))
        ));
    }

    struct WakeCounter(AtomicU64);

    impl std::task::Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn loaded_roots_keep_top_level_assignments_isolated() {
        let mut runtime = runtime(Surface::new());
        let writer = compile(&runtime, "shared = 41\nreturn shared");
        let reader = compile(&runtime, "return shared");
        let writer = runtime
            .load_compiled(&writer, &LoadTarget::named("writer.luau"))
            .expect("writer loads");
        let reader = runtime
            .load_compiled(&reader, &LoadTarget::named("reader.luau"))
            .expect("reader loads");

        assert_eq!(
            number(
                &runtime
                    .run_ready(&writer, CallOptions::new())
                    .expect("writer runs")
            ),
            41
        );
        assert_eq!(
            runtime
                .run_ready(&reader, CallOptions::new())
                .expect("reader runs"),
            vec![ValueSnapshot::Nil]
        );
    }

    #[test]
    fn typed_stashes_fetch_release_and_never_alias_reused_slots() {
        let mut runtime = runtime(Surface::new());
        let chunk = compile(
            &runtime,
            "return { answer = 42 }, function(x) return x + 1 end",
        );
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("callbacks.luau"))
            .expect("root loads");
        let (table, function, value) = runtime
            .step_root(&root, &CallOptions::new(), |scope, main| {
                let (table, function): (Table<'_>, Function<'_>) = scope.call(main, ())?;
                Ok((
                    scope.stash_table(table)?,
                    scope.stash_function(function)?,
                    scope.stash_value(ScopedValue::Integer(7))?,
                ))
            })
            .expect("root values stash");
        let table = runtime.retain(table);
        let function = runtime.retain(function);
        let value = runtime.retain(value);

        let answer = runtime
            .get(table, &CallOptions::new(), |scope, table| {
                table.get::<_, i64>(scope, "answer")
            })
            .expect("table fetches");
        assert_eq!(answer, 42);
        let incremented = runtime
            .get(&function, &CallOptions::new(), |scope, function| {
                scope.call::<_, f64>(function, 41.0_f64)
            })
            .expect("function fetches");
        assert_eq!(incremented, 42.0);
        let scalar = runtime
            .get(value, &CallOptions::new(), |_scope, value| match value {
                ScopedValue::Integer(value) => Ok(value),
                other => Err(RuntimeError::runtime(format!(
                    "expected integer, got {}",
                    other.type_name()
                ))),
            })
            .expect("value fetches");
        assert_eq!(scalar, 7);

        runtime.release(table).expect("table releases");
        let stale_function = function.clone();
        runtime.release(&function).expect("function releases");
        assert!(matches!(
            runtime.get(
                &stale_function,
                &CallOptions::new(),
                |_scope, _function| Ok(())
            ),
            Err(LifecycleError::StaleHandle {
                kind: HandleKind::Function,
                ..
            })
        ));
        runtime
            .step(&CallOptions::new(), |scope| {
                assert!(matches!(
                    stale_function.resolve(scope),
                    Err(LifecycleError::StaleHandle {
                        kind: HandleKind::Function,
                        ..
                    })
                ));
                Ok(())
            })
            .expect("released function rejects in-scope resolution");
        runtime.release(value).expect("value releases");
        assert!(matches!(
            runtime.release(table),
            Err(LifecycleError::StaleHandle {
                kind: HandleKind::Table,
                ..
            })
        ));

        let replacement = runtime
            .step(&CallOptions::new(), |scope| {
                let table = scope.create_table()?;
                scope.stash_table(table)
            })
            .expect("replacement stashes");
        let replacement = runtime.retain(replacement);
        assert_ne!(table, replacement, "a released generation never aliases");

        let stale_root = root.clone();
        runtime.unload(&root).expect("root unloads");
        assert!(matches!(
            runtime.run_ready(&stale_root, CallOptions::new()),
            Err(LifecycleError::StaleHandle {
                kind: HandleKind::Root,
                ..
            })
        ));
        runtime
            .step(&CallOptions::new(), |scope| {
                assert!(matches!(
                    stale_root.resolve(scope),
                    Err(LifecycleError::StaleHandle {
                        kind: HandleKind::Root,
                        ..
                    })
                ));
                Ok(())
            })
            .expect("unloaded root rejects in-scope resolution");
    }

    #[test]
    fn shared_handle_keys_reject_wrong_generation_and_epoch_for_every_arena() {
        let mut runtime = runtime(Surface::new());
        let chunk = compile(&runtime, "return {}, function() end");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("identity.luau"))
            .expect("root loads");
        let (table, function, value) = runtime
            .step_root(&root, &CallOptions::new(), |scope, main| {
                let (table, function): (Table<'_>, Function<'_>) = scope.call(main, ())?;
                Ok((
                    scope.stash_table(table)?,
                    scope.stash_function(function)?,
                    scope.stash_value(ScopedValue::Integer(1))?,
                ))
            })
            .expect("values stash");
        let table = runtime.retain(table);
        let function = runtime.retain(function);
        let value = runtime.retain(value);

        for wrong_epoch in [false, true] {
            let alter = |mut key: HandleKey| {
                if wrong_epoch {
                    key.source_epoch = key.source_epoch.wrapping_add(1);
                } else {
                    key.generation = next_generation(key.generation);
                }
                key
            };
            assert!(
                Runtime::retained(
                    runtime.id,
                    runtime.source_epoch,
                    &runtime.roots,
                    alter(root.key),
                    HandleKind::Root,
                )
                .is_err()
            );
            assert!(
                Runtime::retained(
                    runtime.id,
                    runtime.source_epoch,
                    &runtime.values,
                    alter(value.key),
                    HandleKind::Value,
                )
                .is_err()
            );
            assert!(
                Runtime::retained(
                    runtime.id,
                    runtime.source_epoch,
                    &runtime.tables,
                    alter(table.key),
                    HandleKind::Table,
                )
                .is_err()
            );
            assert!(
                Runtime::retained(
                    runtime.id,
                    runtime.source_epoch,
                    &runtime.functions,
                    alter(function.key),
                    HandleKind::Function,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn resolvable_functions_distinguish_cross_vm_and_dropped_runtime() {
        let mut owner = runtime(Surface::new());
        let chunk = compile(&owner, "return function() return 1 end");
        let root = owner
            .load_compiled(&chunk, &LoadTarget::named("owner.luau"))
            .expect("root loads");
        let function = owner
            .step_root(&root, &CallOptions::new(), |scope, main| {
                let function: Function<'_> = scope.call(main, ())?;
                scope.stash_function(function)
            })
            .expect("function stashes");
        let function = owner.retain(function);
        let mut other = runtime(Surface::new());

        other
            .step(&CallOptions::new(), |scope| {
                assert!(matches!(
                    root.resolve(scope),
                    Err(LifecycleError::Runtime(_))
                ));
                assert!(matches!(
                    function.resolve(scope),
                    Err(LifecycleError::Runtime(_))
                ));
                Ok(())
            })
            .expect("cross-VM resolution reports runtime ownership");

        drop(owner);
        other
            .step(&CallOptions::new(), |scope| {
                assert!(matches!(
                    root.resolve(scope),
                    Err(LifecycleError::StaleHandle {
                        kind: HandleKind::Root,
                        ..
                    })
                ));
                assert!(matches!(
                    function.resolve(scope),
                    Err(LifecycleError::StaleHandle {
                        kind: HandleKind::Function,
                        ..
                    })
                ));
                Ok(())
            })
            .expect("dropped owner leaves stale resolvable handles");
    }

    struct MutableSource {
        source: Mutex<Vec<u8>>,
        epoch: AtomicU64,
    }

    impl MutableSource {
        fn new(source: &str) -> Self {
            Self {
                source: Mutex::new(source.as_bytes().to_vec()),
                epoch: AtomicU64::new(1),
            }
        }

        fn set(&self, source: &str) {
            *self.source.lock().expect("source lock") = source.as_bytes().to_vec();
            self.epoch.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl SyncSourceProvider for MutableSource {
        fn resolve_sync(
            &self,
            requester: Option<&ModuleId>,
            request: &[u8],
        ) -> SourceResult<ModuleId> {
            if request == b"./dep" && requester.and_then(ModuleId::as_str) == Some("app/main") {
                return Ok(ModuleId::new("app/dep"));
            }
            Err(SourceError::MissingModule {
                id: ModuleId::from(request),
            })
        }

        fn read_sync(&self, id: &ModuleId) -> SourceResult<Vec<u8>> {
            if id.as_str() == Some("app/dep") {
                return Ok(self.source.lock().expect("source lock").clone());
            }
            Err(SourceError::MissingModule { id: id.clone() })
        }

        fn metadata(&self, id: &ModuleId) -> SourceMetadata {
            SourceMetadata::new(format!("{}.luau", id.to_lossy_string()))
        }

        fn epoch(&self) -> u64 {
            self.epoch.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn source_epoch_invalidation_releases_roots_stashes_and_module_cache() {
        let source = Arc::new(MutableSource::new("return 1"));
        let surface = Surface::builder()
            .module_source(source.clone())
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(&runtime, "return require('./dep')");
        let target = LoadTarget::module_id(ModuleId::new("app/main"));
        let root = runtime.load_compiled(&chunk, &target).expect("root loads");
        assert_eq!(
            number(
                &runtime
                    .run_ready(&root, CallOptions::new())
                    .expect("root runs")
            ),
            1
        );
        let stash = runtime
            .step(&CallOptions::new(), |scope| {
                scope.stash_value(ScopedValue::Integer(9))
            })
            .expect("value stashes");
        let value = runtime.retain(stash);

        source.set("return 2");
        let invalidation = runtime
            .invalidate_if_source_changed()
            .expect("epoch change invalidates");
        assert_eq!(invalidation.roots, 1);
        assert_eq!(invalidation.values, 1);
        assert_eq!(invalidation.previous_epoch, 1);
        assert_eq!(invalidation.current_epoch, 2);
        assert!(matches!(
            runtime.run_ready(&root, CallOptions::new()),
            Err(LifecycleError::StaleHandle { .. })
        ));
        assert!(matches!(
            runtime.release(value),
            Err(LifecycleError::StaleHandle { .. })
        ));

        let root = runtime
            .load_compiled(&chunk, &target)
            .expect("root reloads explicitly");
        assert_eq!(
            number(
                &runtime
                    .run_ready(&root, CallOptions::new())
                    .expect("new root runs")
            ),
            2
        );

        let prepared_source = Source::text(ModuleId::new("app/main"), "return require('./dep')");
        let prepared = runtime
            .prepare_ready(prepared_source, PrepareOptions::new())
            .expect("graph prepares");
        source.set("return 3");
        assert!(matches!(
            runtime.load_prepared(&prepared),
            Err(LifecycleError::PreparedLoad(_))
        ));
    }

    #[test]
    fn module_domains_isolate_exports_and_release_independently() {
        let source = Arc::new(MutableSource::new("return { count = 0 }"));
        let surface = Surface::builder()
            .module_source(source)
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(
            &runtime,
            "local dep = require('./dep')\ndep.count += 1\nreturn dep.count",
        );
        let target = LoadTarget::module_id(ModuleId::new("app/main"));
        let first_domain = runtime.create_module_domain();
        let second_domain = runtime.create_module_domain();
        let first = runtime
            .load_compiled_in(first_domain, &chunk, &target)
            .expect("first domain root loads");
        let second = runtime
            .load_compiled_in(second_domain, &chunk, &target)
            .expect("second domain root loads");

        assert_eq!(
            number(
                &runtime
                    .run_ready(&first, CallOptions::new())
                    .expect("first domain runs")
            ),
            1
        );
        assert_eq!(
            number(
                &runtime
                    .run_ready(&first, CallOptions::new())
                    .expect("first domain reuses exports")
            ),
            2
        );
        assert_eq!(
            number(
                &runtime
                    .run_ready(&second, CallOptions::new())
                    .expect("second domain has isolated exports")
            ),
            1
        );

        assert!(matches!(
            runtime.release_module_domain(first_domain),
            Err(LifecycleError::InUse {
                kind: HandleKind::ModuleDomain
            })
        ));
        runtime.unload(&first).expect("first root unloads");
        assert_eq!(
            runtime
                .release_module_domain(first_domain)
                .expect("first domain releases"),
            ModuleDomainRelease {
                cached_modules: 1,
                in_flight_modules: 0
            }
        );
        assert_eq!(
            number(
                &runtime
                    .run_ready(&second, CallOptions::new())
                    .expect("second domain remains live")
            ),
            2
        );
        assert!(matches!(
            runtime.release_module_domain(first_domain),
            Err(LifecycleError::StaleHandle {
                kind: HandleKind::ModuleDomain,
                ..
            })
        ));

        runtime.unload(&second).expect("second root unloads");
        runtime
            .release_module_domain(second_domain)
            .expect("second domain releases");
        let default_domain = runtime.default_module_domain();
        assert!(matches!(
            runtime.release_module_domain(default_domain),
            Err(LifecycleError::PermanentHandle {
                kind: HandleKind::ModuleDomain
            })
        ));
    }

    #[test]
    fn module_domains_reject_foreign_handles_and_invalidate_globally() {
        let source = Arc::new(MutableSource::new("return 1"));
        let surface = Surface::builder()
            .module_source(source.clone())
            .build()
            .expect("surface builds");
        let mut owner = runtime(surface);
        let domain = owner.create_module_domain();
        let mut foreign = runtime(Surface::new());
        assert!(matches!(
            foreign.release_module_domain(domain),
            Err(LifecycleError::StaleHandle {
                kind: HandleKind::ModuleDomain,
                ..
            })
        ));

        source.set("return 2");
        let invalidation = owner
            .invalidate_if_source_changed()
            .expect("source change invalidates");
        assert_eq!(invalidation.module_domains, 2);
        assert!(matches!(
            owner.release_module_domain(domain),
            Err(LifecycleError::StaleHandle {
                kind: HandleKind::ModuleDomain,
                ..
            })
        ));
        let default = owner.default_module_domain();
        assert_eq!(default.source_epoch(), 2);
        assert!(matches!(
            owner.release_module_domain(default),
            Err(LifecycleError::PermanentHandle {
                kind: HandleKind::ModuleDomain
            })
        ));
    }

    #[test]
    fn repeated_module_domain_lifecycles_reach_a_heap_plateau() {
        let source = Arc::new(MutableSource::new("return { answer = 42 }"));
        let surface = Surface::builder()
            .module_source(source)
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(&runtime, "return require('./dep').answer");
        let target = LoadTarget::module_id(ModuleId::new("app/main"));
        let mut after_warmup = 0;

        for iteration in 0..100 {
            let domain = runtime.create_module_domain();
            let root = runtime
                .load_compiled_in(domain, &chunk, &target)
                .expect("domain root loads");
            assert_eq!(
                number(
                    &runtime
                        .run_ready(&root, CallOptions::new())
                        .expect("domain root runs")
                ),
                42
            );
            runtime.unload(&root).expect("domain root unloads");
            assert_eq!(
                runtime
                    .release_module_domain(domain)
                    .expect("domain releases")
                    .cached_modules,
                1
            );
            if iteration == 10 {
                assert!(runtime.vm.collect().completed());
                after_warmup = runtime.heap_used_bytes();
            }
        }

        assert!(runtime.vm.collect().completed());
        let final_bytes = runtime.heap_used_bytes();
        assert!(
            final_bytes <= after_warmup + 64 * 1024,
            "module domain churn should plateau: warm={after_warmup}, final={final_bytes}"
        );
    }

    #[test]
    fn root_steps_compose_nested_calls_and_non_send_borrowed_context() {
        let mut runtime = runtime(Surface::new());
        let chunk = compile(
            &runtime,
            "local function inner(x) return x + 1 end\nreturn function(x) return inner(x) end",
        );
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("nested.luau"))
            .expect("root loads");
        let callback = runtime
            .step_root(&root, &CallOptions::new(), |scope, main| {
                let callback: Function<'_> = scope.call(main, ())?;
                scope.stash_function(callback)
            })
            .expect("callback stashes");
        let callback = runtime.retain(callback);
        let mut count = ContextCounter(0);
        let value = runtime
            .step_with_context(&mut count, &CallOptions::new(), |scope| {
                scope.context_mut::<ContextCounter>().expect("context").0 += 1;
                let root_value = root
                    .resolve(scope)
                    .map_err(|error| RuntimeError::runtime(error.to_string()))?;
                let nested_callback: Function<'_> = scope.call(root_value, ())?;
                assert_eq!(scope.call::<_, f64>(nested_callback, 4.0_f64)?, 5.0);
                let callback = callback
                    .resolve(scope)
                    .map_err(|error| RuntimeError::runtime(error.to_string()))?;
                assert_eq!(scope.call::<_, f64>(callback, 9.0_f64)?, 10.0);
                Ok(())
            })
            .and_then(|()| {
                runtime.get(&callback, &CallOptions::new(), |scope, callback| {
                    scope.call::<_, f64>(callback, 41.0_f64)
                })
            })
            .expect("nested callback runs");
        assert_eq!(value, 42.0);
        assert_eq!(count.0, 1);

        let mut foreign = Runtime::new(
            Surface::new(),
            &VmConfig::untrusted(ruau_vm::Ambient::deterministic(0), Limits::unlimited()),
        )
        .expect("foreign runtime builds");
        foreign
            .step(&CallOptions::new(), |scope| {
                assert!(matches!(
                    root.resolve(scope),
                    Err(LifecycleError::Runtime(_))
                ));
                assert!(matches!(
                    callback.resolve(scope),
                    Err(LifecycleError::Runtime(_))
                ));
                Ok(())
            })
            .expect("foreign scope rejects retained handles");
    }

    #[tokio::test]
    async fn async_root_runs_with_borrowed_context() {
        let surface = Surface::builder()
            .module(Arc::new(ContextModule))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(&runtime, "return read_context_counter()");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("context.luau"))
            .expect("root loads");
        let mut context = ContextCounter(40);
        let values = runtime
            .run_with_context(&root, &mut context, CallOptions::new())
            .await
            .expect("async root runs with context");
        assert_eq!(number(&values), 41);
        assert_eq!(context.0, 41);
    }

    #[tokio::test]
    async fn async_runs_and_repeated_reload_cleanup_stay_bounded() {
        let mut runtime = runtime(Surface::new());
        let chunk = compile(&runtime, "return 42");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("async.luau"))
            .expect("root loads");
        assert_eq!(
            number(
                &runtime
                    .run(&root, CallOptions::new())
                    .await
                    .expect("async root runs")
            ),
            42
        );
        runtime.unload(&root).expect("root unloads");

        let options = CallOptions::new().limits(Limits::unlimited());
        let mut after_warmup = 0;
        for iteration in 0..100 {
            let root = runtime
                .load_compiled(&chunk, &LoadTarget::named("reload.luau"))
                .expect("root loads");
            assert_eq!(
                number(&runtime.run_ready(&root, CallOptions::new()).expect("runs")),
                42
            );
            runtime.unload(&root).expect("root unloads");
            runtime
                .step(&options, |_scope| Ok(()))
                .expect("cleanup step");
            if iteration == 10 {
                assert!(runtime.vm.collect().completed());
                after_warmup = runtime.heap_used_bytes();
            }
        }
        assert!(runtime.vm.collect().completed());
        let final_bytes = runtime.heap_used_bytes();
        assert!(
            final_bytes <= after_warmup + 64 * 1024,
            "repeated reloads should not grow registry/proto state without bound: warm={after_warmup}, final={final_bytes}"
        );
        assert!(runtime.execution_count() >= 101);
    }

    #[tokio::test]
    async fn detached_invocations_interleave_without_borrowing_the_runtime() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let pending_chunk = compile(&runtime, "return pause(1)");
        let immediate_chunk = compile(&runtime, "return 2");
        let pending_root = runtime
            .load_compiled(&pending_chunk, &LoadTarget::named("pending.luau"))
            .expect("pending root loads");
        let immediate_root = runtime
            .load_compiled(&immediate_chunk, &LoadTarget::named("immediate.luau"))
            .expect("immediate root loads");
        let pending = runtime
            .create_root_invocation(&pending_root)
            .expect("pending invocation starts");
        let immediate = runtime
            .create_root_invocation(&immediate_root)
            .expect("immediate invocation starts");

        assert!(poll_once(&mut runtime, pending).is_pending());
        let Poll::Ready(Ok(values)) = poll_once(&mut runtime, immediate) else {
            panic!("unrelated invocation should complete");
        };
        assert_eq!(number(&values), 2);
        pause_gate.notify_one();
        let Poll::Ready(Ok(values)) = poll_once(&mut runtime, pending) else {
            panic!("pending invocation should resume");
        };
        assert_eq!(number(&values), 1);
        assert!(matches!(
            poll_once(&mut runtime, pending),
            Poll::Ready(Err(LifecycleError::StaleHandle {
                kind: HandleKind::Invocation,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn detached_poll_lends_non_send_context_for_one_segment() {
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::new(tokio::sync::Notify::new()),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(&runtime, "return read_detached_context()");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("context.luau"))
            .expect("root loads");
        let invocation = runtime
            .create_root_invocation(&root)
            .expect("invocation starts");
        let mut host_context = ContextCounter(40);
        let waker = std::task::Waker::noop();
        let mut task_context = Context::from_waker(waker);

        assert!(
            runtime
                .poll_invocation_with_context(
                    invocation,
                    &mut host_context,
                    &CallOptions::new(),
                    &mut task_context,
                )
                .is_pending()
        );
        let Poll::Ready(Ok(values)) = runtime.poll_invocation_with_context(
            invocation,
            &mut host_context,
            &CallOptions::new(),
            &mut task_context,
        ) else {
            panic!("context invocation should resume");
        };
        assert_eq!(number(&values), 41);
        assert_eq!(host_context.0, 41);
    }

    #[test]
    fn detached_completion_can_retain_heap_backed_return_values() {
        let mut runtime = runtime(Surface::new());
        let chunk = compile(&runtime, "return { callback = function() return 42 end }");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("completion.luau"))
            .expect("root loads");
        let invocation = runtime
            .create_root_invocation(&root)
            .expect("invocation starts");
        let mut host_context = ();
        let mut retained = None;
        let waker = std::task::Waker::noop();
        let mut task_context = Context::from_waker(waker);

        let Poll::Ready(Ok(values)) = runtime.poll_invocation_with_context_and_completion(
            invocation,
            &mut host_context,
            &CallOptions::new(),
            &mut task_context,
            |scope, values| {
                let values = values.into_vec();
                let [ScopedValue::Table(result)] = values.as_slice() else {
                    return Err(RuntimeError::external(
                        "completion expected one table result",
                    ));
                };
                let callback: Function<'_> = result.get(scope, "callback")?;
                retained = Some(scope.stash_function(callback)?);
                Ok(())
            },
        ) else {
            panic!("detached invocation should complete");
        };
        assert!(matches!(
            values.as_slice(),
            [ValueSnapshot::Table(entries)]
                if entries.iter().any(|entry| {
                    entry.key == ValueSnapshot::String(b"callback".to_vec())
                        && entry.value == ValueSnapshot::Opaque("function")
                })
        ));

        let callback = runtime.retain(retained.expect("completion retained callback"));
        let value = runtime
            .step(&CallOptions::new(), |scope| {
                let callback = callback
                    .resolve(scope)
                    .map_err(|error| RuntimeError::external(error.to_string()))?;
                scope.call::<_, f64>(callback, ())
            })
            .expect("retained callback runs");
        assert_eq!(value, 42.0);
    }

    #[test]
    fn detached_function_invocation_consumes_retained_arguments() {
        let mut runtime = runtime(Surface::new());
        let chunk = compile(
            &runtime,
            "return function(left, right) return left + right end",
        );
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("callback.luau"))
            .expect("root loads");
        let (function, left, right) = runtime
            .step_root(&root, &CallOptions::new(), |scope, main| {
                let function: Function<'_> = scope.call(main, ())?;
                Ok((
                    scope.stash_function(function)?,
                    scope.stash_value(ScopedValue::Number(20.0))?,
                    scope.stash_value(ScopedValue::Number(22.0))?,
                ))
            })
            .expect("callback and arguments stash");
        let function = runtime.retain(function);
        let left = runtime.retain(left);
        let right = runtime.retain(right);
        let domain = runtime.create_module_domain();
        let invocation = runtime
            .create_function_invocation(domain, &function, vec![left, right])
            .expect("function invocation starts");

        assert!(matches!(
            runtime.release(left),
            Err(LifecycleError::StaleHandle {
                kind: HandleKind::Value,
                ..
            })
        ));
        runtime
            .release(&function)
            .expect("function handle can release after start");
        let values = loop {
            if let Poll::Ready(result) = poll_once(&mut runtime, invocation) {
                break result.expect("function invocation completes");
            }
        };
        assert_eq!(number(&values), 42);
        runtime
            .release_module_domain(domain)
            .expect("function domain releases");
    }

    #[tokio::test]
    async fn aborting_one_detached_invocation_preserves_another() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(
            &runtime,
            "local co = coroutine.create(function() return pause(7) end)\n\
             local ok, value = coroutine.resume(co)\n\
             if not ok then error(value) end\n\
             return value",
        );
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("abort.luau"))
            .expect("root loads");
        let first = runtime
            .create_root_invocation(&root)
            .expect("first invocation starts");
        let second = runtime
            .create_root_invocation(&root)
            .expect("second invocation starts");

        assert!(poll_once(&mut runtime, first).is_pending());
        assert!(poll_once(&mut runtime, second).is_pending());
        runtime
            .abort_invocation(first)
            .expect("first invocation aborts");
        assert!(matches!(
            runtime.abort_invocation(first),
            Err(LifecycleError::StaleHandle {
                kind: HandleKind::Invocation,
                ..
            })
        ));
        pause_gate.notify_one();
        let Poll::Ready(Ok(values)) = poll_once(&mut runtime, second) else {
            panic!("unrelated invocation should resume");
        };
        assert_eq!(number(&values), 7);
    }

    #[tokio::test]
    async fn same_domain_invocations_wait_for_one_module_execution() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let source = Arc::new(MutableSource::new(
            "module_load_count = (module_load_count or 0) + 1\n\
             pause(module_load_count)\n\
             return module_load_count",
        ));
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .module_source(source)
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(&runtime, "return require('./dep')");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::module_id(ModuleId::new("app/main")))
            .expect("root loads");
        let leader = runtime
            .create_root_invocation(&root)
            .expect("leader starts");
        let waiter = runtime
            .create_root_invocation(&root)
            .expect("waiter starts");

        assert!(poll_once(&mut runtime, leader).is_pending());
        let wake_count = Arc::new(WakeCounter(AtomicU64::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);
        assert!(
            runtime
                .poll_invocation(waiter, &CallOptions::new(), &mut context)
                .is_pending()
        );
        pause_gate.notify_one();
        tokio::task::yield_now().await;
        let Poll::Ready(Ok(leader_values)) = poll_once(&mut runtime, leader) else {
            panic!("leader should finish the module load");
        };
        assert_eq!(number(&leader_values), 1);
        assert_eq!(wake_count.0.load(Ordering::Relaxed), 1);
        let Poll::Ready(Ok(waiter_values)) =
            runtime.poll_invocation(waiter, &CallOptions::new(), &mut context)
        else {
            panic!("waiter should reuse the cached module");
        };
        assert_eq!(number(&waiter_values), 1);
    }

    #[tokio::test]
    async fn nested_coroutine_waits_for_in_flight_module_after_await() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let source = Arc::new(MutableSource::new(
            "module_load_count = (module_load_count or 0) + 1\n\
             pause(module_load_count)\n\
             return module_load_count",
        ));
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .module_source(source)
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let leader_chunk = compile(&runtime, "return require('./dep')");
        let waiter_chunk = compile(
            &runtime,
            "local co = coroutine.create(function()\n\
                 pause(0)\n\
                 return require('./dep')\n\
             end)\n\
             local ok, value = coroutine.resume(co)\n\
             if not ok then error(value) end\n\
             return value",
        );
        let target = LoadTarget::module_id(ModuleId::new("app/main"));
        let leader_root = runtime
            .load_compiled(&leader_chunk, &target)
            .expect("leader root loads");
        let waiter_root = runtime
            .load_compiled(&waiter_chunk, &target)
            .expect("waiter root loads");
        let waiter = runtime
            .create_root_invocation(&waiter_root)
            .expect("waiter starts");
        let leader = runtime
            .create_root_invocation(&leader_root)
            .expect("leader starts");

        assert!(poll_once(&mut runtime, waiter).is_pending());
        assert!(poll_once(&mut runtime, leader).is_pending());
        pause_gate.notify_one();
        tokio::task::yield_now().await;
        let waiter_poll = poll_once(&mut runtime, waiter);
        assert!(
            waiter_poll.is_pending(),
            "coroutine should park behind the in-flight module, got {waiter_poll:?}"
        );
        pause_gate.notify_one();
        tokio::task::yield_now().await;
        let Poll::Ready(Ok(leader_values)) = poll_once(&mut runtime, leader) else {
            panic!("leader should finish the module load");
        };
        assert_eq!(number(&leader_values), 1);
        let Poll::Ready(Ok(waiter_values)) = poll_once(&mut runtime, waiter) else {
            panic!("coroutine should resume with the cached module");
        };
        assert_eq!(number(&waiter_values), 1);
    }

    #[tokio::test]
    async fn aborting_coroutine_module_leader_releases_the_loading_marker() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let source = Arc::new(MutableSource::new(
            "module_load_count = (module_load_count or 0) + 1\n\
             pause(module_load_count)\n\
             return module_load_count",
        ));
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .module_source(source)
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let coroutine_chunk = compile(
            &runtime,
            "local co = coroutine.create(function() return require('./dep') end)\n\
             local ok, value = coroutine.resume(co)\n\
             if not ok then error(value) end\n\
             return value",
        );
        let retry_chunk = compile(&runtime, "return require('./dep')");
        let target = LoadTarget::module_id(ModuleId::new("app/main"));
        let coroutine_root = runtime
            .load_compiled(&coroutine_chunk, &target)
            .expect("coroutine root loads");
        let retry_root = runtime
            .load_compiled(&retry_chunk, &target)
            .expect("retry root loads");
        let leader = runtime
            .create_root_invocation(&coroutine_root)
            .expect("coroutine leader starts");

        assert!(poll_once(&mut runtime, leader).is_pending());
        runtime
            .abort_invocation(leader)
            .expect("coroutine leader aborts");

        let retry = runtime
            .create_root_invocation(&retry_root)
            .expect("retry starts");
        assert!(
            poll_once(&mut runtime, retry).is_pending(),
            "retry should become the module leader"
        );
        pause_gate.notify_one();
        tokio::task::yield_now().await;
        let Poll::Ready(Ok(values)) = poll_once(&mut runtime, retry) else {
            panic!("retry should finish the abandoned module load");
        };
        assert_eq!(number(&values), 1);
    }

    #[tokio::test]
    async fn fatal_detached_completion_releases_coroutine_module_state() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let attempts = Arc::new(AtomicU64::new(0));
        let source = Arc::new(MutableSource::new(
            "if next_module_attempt() < 2 then\n\
                 pause(1)\n\
                 while true do end\n\
             end\n\
             return 42",
        ));
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .module(Arc::new(AttemptModule {
                attempts: Arc::clone(&attempts),
            }))
            .module_source(source)
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let coroutine_chunk = compile(
            &runtime,
            "local co = coroutine.create(function() return require('./dep') end)\n\
             local ok, value = coroutine.resume(co)\n\
             if not ok then error(value) end\n\
             return value",
        );
        let retry_chunk = compile(&runtime, "return require('./dep')");
        let target = LoadTarget::module_id(ModuleId::new("app/main"));
        let coroutine_root = runtime
            .load_compiled(&coroutine_chunk, &target)
            .expect("coroutine root loads");
        let retry_root = runtime
            .load_compiled(&retry_chunk, &target)
            .expect("retry root loads");
        let invocation = runtime
            .create_root_invocation(&coroutine_root)
            .expect("coroutine invocation starts");

        let first_poll = poll_once(&mut runtime, invocation);
        assert!(
            first_poll.is_pending(),
            "first module attempt should pause, attempts={}, got {first_poll:?}",
            attempts.load(Ordering::Relaxed)
        );
        pause_gate.notify_one();
        tokio::task::yield_now().await;
        let expired = CallOptions::new().limits(Limits {
            deadline: Some(ruau_vm::Deadline::Logical(100)),
            ..Limits::unlimited()
        });
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(
            runtime.poll_invocation(invocation, &expired, &mut context),
            Poll::Ready(Err(LifecycleError::Exec(ExecError::Stopped(
                ruau_vm::StopReason::Deadline
            ))))
        ));

        let retry = runtime
            .create_root_invocation(&retry_root)
            .expect("retry starts");
        let Poll::Ready(Ok(values)) = poll_once(&mut runtime, retry) else {
            panic!("retry should execute after fatal coroutine cleanup");
        };
        assert_eq!(number(&values), 42);
    }

    #[tokio::test]
    async fn mixed_entry_points_report_in_flight_module_loading() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let source = Arc::new(MutableSource::new(
            "pause(1)\n\
             return 1",
        ));
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .module_source(source)
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(&runtime, "return require('./dep')");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::module_id(ModuleId::new("app/main")))
            .expect("root loads");
        let leader = runtime
            .create_root_invocation(&root)
            .expect("leader starts");

        assert!(poll_once(&mut runtime, leader).is_pending());
        let LifecycleError::Exec(sync_error) = runtime
            .run_ready(&root, CallOptions::new())
            .expect_err("sync entry should fail while the module is loading")
        else {
            panic!("sync entry should return an execution error");
        };
        assert!(
            sync_error
                .message()
                .contains("required module is already loading")
        );
        let LifecycleError::Exec(async_error) = runtime
            .run(&root, CallOptions::new())
            .await
            .expect_err("async entry should fail while the module is loading")
        else {
            panic!("async entry should return an execution error");
        };
        assert!(
            async_error
                .message()
                .contains("required module is already loading")
        );
        runtime
            .abort_invocation(leader)
            .expect("leader invocation aborts");
    }

    #[tokio::test]
    async fn aborting_module_leader_wakes_a_same_domain_waiter() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let source = Arc::new(MutableSource::new(
            "module_load_count = (module_load_count or 0) + 1\n\
             pause(module_load_count)\n\
             return module_load_count",
        ));
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .module_source(source)
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(&runtime, "return require('./dep')");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::module_id(ModuleId::new("app/main")))
            .expect("root loads");
        let leader = runtime
            .create_root_invocation(&root)
            .expect("leader starts");
        let waiter = runtime
            .create_root_invocation(&root)
            .expect("waiter starts");

        assert!(poll_once(&mut runtime, leader).is_pending());
        assert!(poll_once(&mut runtime, waiter).is_pending());
        runtime
            .abort_invocation(leader)
            .expect("leader invocation aborts");
        assert!(poll_once(&mut runtime, waiter).is_pending());
        pause_gate.notify_one();
        tokio::task::yield_now().await;
        let Poll::Ready(Ok(values)) = poll_once(&mut runtime, waiter) else {
            panic!("waiter should retry the abandoned module load");
        };
        assert_eq!(number(&values), 2);
    }

    #[tokio::test]
    async fn detached_invocation_services_nested_protected_callbacks() {
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::new(tokio::sync::Notify::new()),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(
            &runtime,
            "function detached_callback(value)\n\
                 return value * 3\n\
             end\n\
             return call_detached_callback(7)",
        );
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("protected-callback.luau"))
            .expect("root loads");
        let invocation = runtime
            .create_root_invocation(&root)
            .expect("invocation starts");

        for _ in 0..10 {
            match poll_once(&mut runtime, invocation) {
                Poll::Pending => tokio::task::yield_now().await,
                Poll::Ready(Ok(values)) => {
                    assert_eq!(number(&values), 21);
                    return;
                }
                Poll::Ready(Err(error)) => panic!("protected callback failed: {error:?}"),
            }
        }
        panic!("protected callback did not complete");
    }

    #[tokio::test]
    async fn detached_nested_coroutine_failure_keeps_its_primary_frame() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(
            &runtime,
            "local wrapped = coroutine.wrap(function()\n\
                 pause(1)\n\
                 error('nested boom')\n\
             end)\n\
             return wrapped()",
        );
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("nested-failure.luau"))
            .expect("root loads");
        let invocation = runtime
            .create_root_invocation(&root)
            .expect("invocation starts");

        assert!(poll_once(&mut runtime, invocation).is_pending());
        pause_gate.notify_one();
        tokio::task::yield_now().await;
        let Poll::Ready(Err(LifecycleError::Exec(ExecError::Script(error)))) =
            poll_once(&mut runtime, invocation)
        else {
            panic!("nested coroutine should fail as a script error");
        };
        let frame = error
            .frames()
            .iter()
            .find(|frame| frame.chunk_name().contains("nested-failure.luau"))
            .expect("nested coroutine source frame is preserved");
        assert!(frame.line_number().is_some());
        assert!(
            error
                .traceback()
                .is_some_and(|traceback| traceback.contains("nested-failure.luau"))
        );
    }

    #[tokio::test]
    async fn panic_during_one_detached_poll_poison_fails_other_invocations_closed() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let pending_chunk = compile(&runtime, "return pause(1)");
        let panic_chunk = compile(&runtime, "return panic_detached()");
        let pending_root = runtime
            .load_compiled(&pending_chunk, &LoadTarget::named("pending-poison.luau"))
            .expect("pending root loads");
        let panic_root = runtime
            .load_compiled(&panic_chunk, &LoadTarget::named("panic-poison.luau"))
            .expect("panic root loads");
        let pending = runtime
            .create_root_invocation(&pending_root)
            .expect("pending invocation starts");
        let panicking = runtime
            .create_root_invocation(&panic_root)
            .expect("panicking invocation starts");

        assert!(poll_once(&mut runtime, pending).is_pending());
        let panicking_result = loop {
            match poll_once(&mut runtime, panicking) {
                Poll::Pending => tokio::task::yield_now().await,
                Poll::Ready(result) => break result,
            }
        };
        assert!(matches!(
            panicking_result,
            Err(LifecycleError::Exec(ExecError::PanicPoison))
        ));
        assert!(matches!(
            poll_once(&mut runtime, pending),
            Poll::Ready(Err(LifecycleError::Exec(ExecError::PanicPoison)))
        ));
    }

    #[tokio::test]
    async fn pending_detached_host_work_ignores_later_poll_deadlines() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(&runtime, "return pause(4)");
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("pending-deadline.luau"))
            .expect("root loads");
        let invocation = runtime
            .create_root_invocation(&root)
            .expect("invocation starts");

        assert!(poll_once(&mut runtime, invocation).is_pending());
        let expired = CallOptions::new().limits(Limits {
            deadline: Some(ruau_vm::Deadline::Wall(std::time::Instant::now())),
            ..Limits::unlimited()
        });
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(
            runtime
                .poll_invocation(invocation, &expired, &mut context)
                .is_pending()
        );
        pause_gate.notify_one();
        tokio::task::yield_now().await;
        let Poll::Ready(Ok(values)) =
            runtime.poll_invocation(invocation, &CallOptions::new(), &mut context)
        else {
            panic!("host operation should complete without a parked deadline");
        };
        assert_eq!(number(&values), 4);
    }

    #[tokio::test]
    async fn repeated_detached_suspend_and_abort_reaches_a_heap_plateau() {
        let pause_gate = Arc::new(tokio::sync::Notify::new());
        let surface = Surface::builder()
            .module(Arc::new(DetachedModule {
                pause_gate: Arc::clone(&pause_gate),
            }))
            .build()
            .expect("surface builds");
        let mut runtime = runtime(surface);
        let chunk = compile(
            &runtime,
            "local co = coroutine.create(function() return pause(1) end)\n\
             local ok, value = coroutine.resume(co)\n\
             if not ok then error(value) end\n\
             return value",
        );
        let root = runtime
            .load_compiled(&chunk, &LoadTarget::named("abort-plateau.luau"))
            .expect("root loads");
        let mut after_warmup = 0;

        for iteration in 0..100 {
            let invocation = runtime
                .create_root_invocation(&root)
                .expect("invocation starts");
            assert!(poll_once(&mut runtime, invocation).is_pending());
            runtime
                .abort_invocation(invocation)
                .expect("invocation aborts");
            runtime
                .step(&CallOptions::new().limits(Limits::unlimited()), |_scope| {
                    Ok(())
                })
                .expect("cleanup step");
            if iteration == 10 {
                assert!(runtime.vm.collect().completed());
                after_warmup = runtime.heap_used_bytes();
            }
        }

        assert!(runtime.vm.collect().completed());
        let final_bytes = runtime.heap_used_bytes();
        assert!(
            final_bytes <= after_warmup + 64 * 1024,
            "detached abort churn should plateau: warm={after_warmup}, final={final_bytes}"
        );
    }

    #[test]
    fn contained_panic_poisoning_is_reported_by_later_core_operations() {
        let mut runtime = Runtime::new(
            Surface::new(),
            &VmConfig::untrusted(ruau_vm::Ambient::deterministic(0), Limits::unlimited()),
        )
        .expect("runtime builds");
        let error = runtime
            .step(&CallOptions::new(), |_scope| -> Result<(), RuntimeError> {
                panic!("contained panic");
            })
            .expect_err("panic poisons");
        assert!(matches!(error, LifecycleError::Runtime(_)));
        assert!(runtime.step(&CallOptions::new(), |_scope| Ok(())).is_err());
    }
}
