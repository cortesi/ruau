//! Checked source preparation: the policy-gated check-then-compile pipeline
//! producing [`PreparedSource`] artifacts.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    sync::Arc,
};

use ruau_bytecode::{BytecodeChunk, CompileError, CompileOptions};
use ruau_source::{
    ModuleId, SnapshotSource, Source, SourceError, SourceGraphSnapshot, SourceProvider,
    SourceResolutionEdge,
};
use ruau_typecheck::{CheckedGraph, Config, Diagnostics, GraphDiagnostics, GraphLimits};
use ruau_vm::{
    CallOptions, ExecError, LoadError, LoadedModule, RuntimeCapabilities, RuntimeCompileContext,
    RuntimeCompiler, ValueSnapshot, Vm,
};

use crate::{CheckOptions, GraphCheckError, GraphCheckOptions, GraphRoot, Surface};

/// Diagnostic gate used by [`Surface::prepare_with_options`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PrepareDiagnosticPolicy {
    /// Reject error-severity diagnostics and preserve warning diagnostics.
    #[default]
    RejectErrors,
    /// Reject any diagnostic, including warnings.
    RejectIssues,
    /// Compile even when checking produced diagnostics.
    AllowDiagnostics,
}

impl PrepareDiagnosticPolicy {
    fn accepts(self, diagnostics: &Diagnostics) -> bool {
        match self {
            Self::RejectErrors => !diagnostics.has_errors(),
            Self::RejectIssues => !diagnostics.has_issues(),
            Self::AllowDiagnostics => true,
        }
    }

    fn accepts_graph(self, diagnostics: &GraphDiagnostics) -> bool {
        match self {
            Self::RejectErrors => !diagnostics.has_errors(),
            Self::RejectIssues => !diagnostics.has_issues(),
            Self::AllowDiagnostics => true,
        }
    }
}

impl fmt::Display for PrepareDiagnosticPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RejectErrors => formatter.write_str("reject errors"),
            Self::RejectIssues => formatter.write_str("reject diagnostics"),
            Self::AllowDiagnostics => formatter.write_str("allow diagnostics"),
        }
    }
}

/// Configuration for checked source preparation.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PrepareOptions {
    diagnostic_policy: PrepareDiagnosticPolicy,
    check_config: Config,
    compile_options: CompileOptions,
    graph_limits: GraphLimits,
}

impl PrepareOptions {
    /// Creates default preparation options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the diagnostic policy.
    #[must_use]
    pub const fn diagnostic_policy(&self) -> PrepareDiagnosticPolicy {
        self.diagnostic_policy
    }

    /// Returns the checker configuration.
    #[must_use]
    pub const fn check_config(&self) -> &Config {
        &self.check_config
    }

    /// Returns the public VM compile policy.
    #[must_use]
    pub const fn compile_options(&self) -> &CompileOptions {
        &self.compile_options
    }

    /// Returns the finite graph traversal limits.
    #[must_use]
    pub const fn graph_limits(&self) -> GraphLimits {
        self.graph_limits
    }

    /// Replaces the diagnostic policy.
    #[must_use]
    pub const fn with_diagnostic_policy(mut self, policy: PrepareDiagnosticPolicy) -> Self {
        self.diagnostic_policy = policy;
        self
    }

    /// Rejects error-severity diagnostics and preserves warnings.
    #[must_use]
    pub const fn reject_errors(self) -> Self {
        self.with_diagnostic_policy(PrepareDiagnosticPolicy::RejectErrors)
    }

    /// Rejects any diagnostic, including warnings.
    #[must_use]
    pub const fn reject_issues(self) -> Self {
        self.with_diagnostic_policy(PrepareDiagnosticPolicy::RejectIssues)
    }

    /// Compiles even when checking produced diagnostics.
    #[must_use]
    pub const fn allow_diagnostics(self) -> Self {
        self.with_diagnostic_policy(PrepareDiagnosticPolicy::AllowDiagnostics)
    }

    /// Replaces the checker configuration.
    ///
    /// If the config does not force a source mode, the surface analysis mode is
    /// still applied before checking.
    #[must_use]
    pub fn with_check_config(mut self, config: Config) -> Self {
        self.check_config = config;
        self
    }

    /// Replaces the public VM compile policy.
    #[must_use]
    pub fn with_compile_options(mut self, options: CompileOptions) -> Self {
        self.compile_options = options;
        self
    }

    /// Replaces the finite graph traversal limits.
    #[must_use]
    pub const fn with_graph_limits(mut self, limits: GraphLimits) -> Self {
        self.graph_limits = limits;
        self
    }
}

/// A checked and compiled source artifact ready to load into a matching VM.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedSource {
    root: PreparedRoot,
    diagnostics: Diagnostics,
}

#[derive(Clone, Debug, PartialEq)]
struct PreparedRoot {
    source: Source,
    chunk: BytecodeChunk,
    runtime_capabilities: RuntimeCapabilities,
}

impl PreparedRoot {
    fn validate_vm(&self, vm: &Vm) -> Result<(), PreparedContextError> {
        if vm.runtime_capabilities() != &self.runtime_capabilities {
            return Err(PreparedContextError::RuntimeCapabilitiesMismatch);
        }
        Ok(())
    }

    fn load(&self, vm: &mut Vm) -> Result<LoadedModule, PreparedLoadError> {
        self.validate_vm(vm).map_err(PreparedLoadError::Context)?;
        let load_name = self.source.load_name();
        vm.load_named_module(&self.chunk, self.source.id().clone(), &load_name)
            .map_err(PreparedLoadError::Load)
    }
}

impl PreparedSource {
    /// Returns the source identity and bytes used for checking and compilation.
    #[must_use]
    pub const fn source(&self) -> &Source {
        &self.root.source
    }

    /// Returns diagnostics produced during checking.
    #[must_use]
    pub const fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// Returns the compiled bytecode chunk.
    #[must_use]
    pub const fn chunk(&self) -> &BytecodeChunk {
        &self.root.chunk
    }

    /// Returns the runtime capabilities used for compilation.
    #[must_use]
    pub const fn runtime_capabilities(&self) -> &RuntimeCapabilities {
        &self.root.runtime_capabilities
    }

    /// Returns the Lua chunk name bytes for loading this script.
    #[must_use]
    pub fn load_name(&self) -> Vec<u8> {
        self.root.source.load_name()
    }

    /// Loads this prepared source into `vm`, preserving both its traceback
    /// load name and its module requester identity.
    ///
    /// # Errors
    /// Returns [`PreparedLoadError`] when the VM context differs or the chunk
    /// cannot be instantiated.
    pub fn load(&self, vm: &mut Vm) -> Result<LoadedModule, PreparedLoadError> {
        self.root.load(vm)
    }

    /// Loads and executes this prepared source in `vm` with empty call options.
    ///
    /// # Errors
    /// Returns [`PreparedRunError`] when loading or execution fails.
    pub fn run(&self, vm: &mut Vm) -> Result<Vec<ValueSnapshot>, PreparedRunError> {
        self.run_with_options(vm, CallOptions::new())
    }

    /// Loads and executes this prepared source in `vm` with explicit call
    /// options.
    ///
    /// # Errors
    /// Returns [`PreparedRunError`] when loading or execution fails.
    pub fn run_with_options(
        &self,
        vm: &mut Vm,
        options: CallOptions,
    ) -> Result<Vec<ValueSnapshot>, PreparedRunError> {
        let module = self.load(vm).map_err(PreparedRunError::Load)?;
        let result = vm.exec(&module, options).map_err(PreparedRunError::Exec);
        vm.unload(module);
        result
    }

    /// Consumes the artifact and returns its parts.
    #[must_use]
    pub fn into_parts(self) -> (Source, Diagnostics, BytecodeChunk, RuntimeCapabilities) {
        (
            self.root.source,
            self.diagnostics,
            self.root.chunk,
            self.root.runtime_capabilities,
        )
    }

    /// Consumes the artifact and returns its compiled bytecode chunk.
    #[must_use]
    pub fn into_chunk(self) -> BytecodeChunk {
        self.root.chunk
    }
}

/// A checked module graph and compiled root ready to load into a matching VM.
#[derive(Clone)]
pub struct PreparedGraph {
    root: PreparedRoot,
    graph: CheckedGraph,
    modules: Arc<BTreeMap<ModuleId, PreparedGraphModule>>,
    module_source: Option<Arc<dyn SourceProvider>>,
    source_epoch: u64,
}

impl fmt::Debug for PreparedGraph {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedGraph")
            .field("source", &self.root.source)
            .field("graph", &self.graph)
            .field("chunk", &self.root.chunk)
            .field("module_count", &self.modules.len())
            .field("runtime_capabilities", &self.root.runtime_capabilities)
            .field("has_module_source", &self.module_source.is_some())
            .field("source_epoch", &self.source_epoch)
            .finish()
    }
}

impl PreparedGraph {
    /// Returns the root identity and bytes used for graph checking and compilation.
    #[must_use]
    pub const fn source(&self) -> &Source {
        &self.root.source
    }

    /// Returns the checked graph, including dependency order and checked modules.
    #[must_use]
    pub const fn graph(&self) -> &CheckedGraph {
        &self.graph
    }

    /// Returns module-qualified diagnostics produced during graph checking.
    #[must_use]
    pub const fn diagnostics(&self) -> &GraphDiagnostics {
        self.graph.diagnostics()
    }

    /// Returns the compiled root bytecode chunk.
    #[must_use]
    pub const fn chunk(&self) -> &BytecodeChunk {
        &self.root.chunk
    }

    /// Returns the number of dependency modules compiled into this artifact.
    #[must_use]
    pub fn compiled_module_count(&self) -> usize {
        self.modules.len()
    }

    /// Returns the runtime capabilities used for compilation.
    #[must_use]
    pub const fn runtime_capabilities(&self) -> &RuntimeCapabilities {
        &self.root.runtime_capabilities
    }

    /// Returns the module-source epoch captured after graph checking.
    #[must_use]
    pub const fn source_epoch(&self) -> u64 {
        self.source_epoch
    }

    /// Returns whether the source epoch has changed since preparation.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.module_source
            .as_ref()
            .is_some_and(|source| source.epoch() != self.source_epoch)
    }

    /// Atomically seals the exact source closure used by this checked graph.
    ///
    /// The supplied snapshot must be the module-source instance used during
    /// preparation. Sealing verifies every checked module and require edge,
    /// then prevents later resolutions or reads outside that closure.
    ///
    /// # Errors
    /// Returns [`PrepareGraphError`] when the source differs from the prepared
    /// graph, changed epoch, or captured operations disagree with the checked
    /// topology.
    pub fn seal_sources(
        &self,
        sources: &Arc<SnapshotSource>,
    ) -> Result<SourceGraphSnapshot, PrepareGraphError> {
        let provided: Arc<dyn SourceProvider> = sources.clone();
        if !self
            .module_source
            .as_ref()
            .is_some_and(|prepared| Arc::ptr_eq(prepared, &provided))
        {
            return Err(PrepareGraphError::SourceSeal {
                source: Box::new(self.root.source.clone()),
                graph: Box::new(self.graph.clone()),
                error: SourceError::other(
                    "snapshot source differs from the prepared graph module source",
                ),
            });
        }
        let modules = self
            .graph
            .checked_modules()
            .keys()
            .map(ModuleId::from)
            .collect::<BTreeSet<_>>();
        let edges = self
            .graph
            .require_edges()
            .iter()
            .map(|edge| {
                SourceResolutionEdge::new(
                    Some(ModuleId::from(edge.requester())),
                    edge.request(),
                    ModuleId::from(edge.required()),
                )
            })
            .collect::<Vec<_>>();
        sources
            .seal(&self.root.source, &modules, &edges)
            .map_err(|error| PrepareGraphError::SourceSeal {
                source: Box::new(self.root.source.clone()),
                graph: Box::new(self.graph.clone()),
                error,
            })
    }

    /// Returns the Lua chunk name bytes for loading the graph root.
    #[must_use]
    pub fn load_name(&self) -> Vec<u8> {
        self.root.source.load_name()
    }

    /// Validates that `vm` has the capabilities and exact module source used
    /// during preparation.
    ///
    /// # Errors
    /// Returns [`PreparedContextError`] when the source is stale or the VM
    /// does not match the preparation context.
    pub fn validate_vm(&self, vm: &Vm) -> Result<(), PreparedContextError> {
        if let Some(source) = &self.module_source {
            let current_epoch = source.epoch();
            if current_epoch != self.source_epoch {
                return Err(PreparedContextError::StaleSource {
                    prepared_epoch: self.source_epoch,
                    current_epoch,
                });
            }
            if !vm.uses_module_source(source) {
                return Err(PreparedContextError::ModuleSourceMismatch);
            }
        }
        self.root.validate_vm(vm)
    }

    /// Loads this prepared graph root into a matching VM.
    ///
    /// # Errors
    /// Returns [`PreparedLoadError`] when the preparation context is stale,
    /// the VM does not match it, or the compiled root cannot be loaded.
    pub fn load(&self, vm: &mut Vm) -> Result<LoadedModule, PreparedLoadError> {
        self.validate_vm(vm).map_err(PreparedLoadError::Context)?;
        self.root.load(vm)
    }

    /// Loads and executes this prepared graph root with empty call options.
    ///
    /// # Errors
    /// Returns [`PreparedGraphRunError`] when validation, loading, or execution fails.
    pub fn run(&self, vm: &mut Vm) -> Result<Vec<ValueSnapshot>, PreparedGraphRunError> {
        self.run_with_options(vm, CallOptions::new())
    }

    /// Loads and executes this prepared graph root with explicit call options.
    ///
    /// # Errors
    /// Returns [`PreparedGraphRunError`] when validation, loading, or execution fails.
    pub fn run_with_options(
        &self,
        vm: &mut Vm,
        options: CallOptions,
    ) -> Result<Vec<ValueSnapshot>, PreparedGraphRunError> {
        let module = self.load(vm).map_err(PreparedGraphRunError::Load)?;
        let options = match self.runtime_compiler() {
            Some(compiler) => options.runtime_compiler(compiler),
            None => options,
        };
        let result = vm
            .exec(&module, options)
            .map_err(PreparedGraphRunError::Exec);
        vm.unload(module);
        result
    }

    /// Consumes the artifact and returns its root source, checked graph, root
    /// bytecode, runtime capabilities, and source epoch.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Source,
        CheckedGraph,
        BytecodeChunk,
        RuntimeCapabilities,
        u64,
    ) {
        (
            self.root.source,
            self.graph,
            self.root.chunk,
            self.root.runtime_capabilities,
            self.source_epoch,
        )
    }

    /// Returns the graph compiler used only while executing this artifact.
    #[doc(hidden)]
    #[must_use]
    pub fn runtime_compiler(&self) -> Option<Arc<dyn RuntimeCompiler>> {
        self.module_source.is_some().then(|| {
            Arc::new(PreparedGraphCompiler {
                modules: Arc::clone(&self.modules),
            }) as Arc<dyn RuntimeCompiler>
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct PreparedGraphModule {
    source: Vec<u8>,
    chunk: BytecodeChunk,
}

struct PreparedGraphCompiler {
    modules: Arc<BTreeMap<ModuleId, PreparedGraphModule>>,
}

impl RuntimeCompiler for PreparedGraphCompiler {
    fn compile(
        &self,
        source: &[u8],
        context: RuntimeCompileContext,
    ) -> Result<BytecodeChunk, Vec<u8>> {
        context.check_cancelled()?;
        let Some(id) = context.module_id else {
            return Err(b"runtime compilation is outside the prepared graph".to_vec());
        };
        let Some(module) = self.modules.get(&id) else {
            return Err(format!("module '{id}' is outside the prepared graph").into_bytes());
        };
        if module.source != source {
            return Err(
                format!("module '{id}' source differs from the prepared graph").into_bytes(),
            );
        }
        Ok(module.chunk.clone())
    }
}

/// A mismatch between a prepared artifact and the VM or source used to run it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedContextError {
    /// The source epoch changed after graph checking.
    StaleSource {
        /// Epoch captured after graph checking.
        prepared_epoch: u64,
        /// Epoch observed before loading.
        current_epoch: u64,
    },
    /// The VM uses a different module-source instance.
    ModuleSourceMismatch,
    /// The VM's runtime capabilities differ from those used for compilation.
    RuntimeCapabilitiesMismatch,
}

impl fmt::Display for PreparedContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleSource {
                prepared_epoch,
                current_epoch,
            } => write!(
                formatter,
                "prepared source graph is stale (prepared epoch {prepared_epoch}, current epoch {current_epoch})"
            ),
            Self::ModuleSourceMismatch => {
                formatter.write_str("prepared graph VM uses a different module source")
            }
            Self::RuntimeCapabilitiesMismatch => {
                formatter.write_str("prepared graph VM uses different runtime capabilities")
            }
        }
    }
}

impl Error for PreparedContextError {}

/// Error returned while loading a prepared root.
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedLoadError {
    /// Preparation context validation failed.
    Context(PreparedContextError),
    /// Loading the compiled root failed.
    Load(LoadError),
}

impl fmt::Display for PreparedLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(formatter, "prepared context: {error}"),
            Self::Load(error) => write!(formatter, "prepared root load failed: {error}"),
        }
    }
}

impl Error for PreparedLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Load(error) => Some(error),
        }
    }
}

/// Error returned while executing a prepared graph root.
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedGraphRunError {
    /// Validation or loading failed.
    Load(PreparedLoadError),
    /// VM execution failed.
    Exec(ExecError),
}

impl fmt::Display for PreparedGraphRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => write!(formatter, "{error}"),
            Self::Exec(error) => write!(formatter, "prepared graph execution failed: {error}"),
        }
    }
}

impl Error for PreparedGraphRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Load(error) => Some(error),
            Self::Exec(error) => Some(error),
        }
    }
}

/// Error returned by checked graph preparation.
#[derive(Clone, Debug, PartialEq)]
pub enum PrepareGraphError {
    /// A checked module has no retained source observation.
    MissingSourceObservation {
        /// Checked graph and its diagnostics.
        graph: Box<CheckedGraph>,
        /// Module whose checked bytes are unavailable.
        module: ModuleId,
    },
    /// Graph diagnostics were rejected by the selected policy.
    DiagnosticsRejected {
        /// Source that was checked.
        source: Box<Source>,
        /// Checked graph and its diagnostics.
        graph: Box<CheckedGraph>,
        /// Policy that rejected the diagnostics.
        policy: PrepareDiagnosticPolicy,
    },
    /// Compilation failed after graph diagnostics were accepted.
    Compile {
        /// Source that was checked and then compiled.
        source: Box<Source>,
        /// Checked graph and its diagnostics.
        graph: Box<CheckedGraph>,
        /// Compiler failure.
        error: CompileError,
    },
    /// A checked dependency could not be compiled into the prepared graph.
    DependencyCompile {
        /// Root source whose graph was being prepared.
        source: Box<Source>,
        /// Checked graph and its diagnostics.
        graph: Box<CheckedGraph>,
        /// Dependency whose source could not be compiled.
        module: ModuleId,
        /// Compiler failure.
        error: CompileError,
    },
    /// Captured source operations could not be sealed to the checked graph.
    SourceSeal {
        /// Root source whose graph was prepared.
        source: Box<Source>,
        /// Checked graph whose closure was being sealed.
        graph: Box<CheckedGraph>,
        /// Source snapshot failure.
        error: SourceError,
    },
    /// A hard graph traversal limit was exceeded.
    GraphCheck {
        /// Root source whose graph was being checked.
        source: Box<Source>,
        /// Structured graph-checking failure.
        error: GraphCheckError,
    },
}

impl PrepareGraphError {
    /// Returns the source that failed preparation, when source acquisition completed.
    #[must_use]
    pub const fn script_source(&self) -> Option<&Source> {
        match self {
            Self::MissingSourceObservation { .. } => None,
            Self::DiagnosticsRejected { source, .. }
            | Self::Compile { source, .. }
            | Self::DependencyCompile { source, .. }
            | Self::SourceSeal { source, .. }
            | Self::GraphCheck { source, .. } => Some(source),
        }
    }

    /// Returns the checked graph when checking completed.
    #[must_use]
    pub const fn graph(&self) -> Option<&CheckedGraph> {
        match self {
            Self::GraphCheck { .. } => None,
            Self::MissingSourceObservation { graph, .. }
            | Self::DiagnosticsRejected { graph, .. }
            | Self::Compile { graph, .. }
            | Self::DependencyCompile { graph, .. }
            | Self::SourceSeal { graph, .. } => Some(graph),
        }
    }

    /// Returns graph diagnostics when checking completed.
    #[must_use]
    pub fn diagnostics(&self) -> Option<&GraphDiagnostics> {
        self.graph().map(CheckedGraph::diagnostics)
    }

    /// Returns the compiler failure, if compilation stopped preparation.
    #[must_use]
    pub const fn compile_error(&self) -> Option<&CompileError> {
        match self {
            Self::Compile { error, .. } | Self::DependencyCompile { error, .. } => Some(error),
            Self::MissingSourceObservation { .. }
            | Self::DiagnosticsRejected { .. }
            | Self::SourceSeal { .. }
            | Self::GraphCheck { .. } => None,
        }
    }
}

impl fmt::Display for PrepareGraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSourceObservation { module, .. } => {
                write!(
                    formatter,
                    "checked graph has no source observation for {module}"
                )
            }
            Self::DiagnosticsRejected {
                source,
                graph,
                policy,
            } => write!(
                formatter,
                "{} rejected by graph diagnostic policy '{policy}' ({} diagnostics)",
                source.display_name(),
                graph.diagnostics().len()
            ),
            Self::Compile { source, error, .. } => {
                write!(
                    formatter,
                    "compile graph root {}: {error}",
                    source.display_name()
                )
            }
            Self::DependencyCompile { module, error, .. } => {
                write!(formatter, "compile graph dependency {module}: {error}")
            }
            Self::SourceSeal { error, .. } => write!(formatter, "seal source graph: {error}"),
            Self::GraphCheck { error, .. } => write!(formatter, "check source graph: {error}"),
        }
    }
}

impl Error for PrepareGraphError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Compile { error, .. } | Self::DependencyCompile { error, .. } => Some(error),
            Self::SourceSeal { error, .. } => Some(error),
            Self::GraphCheck { error, .. } => Some(error),
            Self::MissingSourceObservation { .. } | Self::DiagnosticsRejected { .. } => None,
        }
    }
}

/// Error returned by checked preparation.
#[derive(Clone, Debug, PartialEq)]
pub enum PrepareError {
    /// Checking produced diagnostics rejected by the selected policy.
    DiagnosticsRejected {
        /// Source that was checked.
        source: Box<Source>,
        /// Diagnostics produced by the checker.
        diagnostics: Diagnostics,
        /// Policy that rejected those diagnostics.
        policy: PrepareDiagnosticPolicy,
    },
    /// Compilation failed after diagnostics were accepted.
    Compile {
        /// Source that was checked and then compiled.
        source: Box<Source>,
        /// Diagnostics produced by the checker before compilation.
        diagnostics: Diagnostics,
        /// Compiler failure.
        error: CompileError,
    },
}

impl PrepareError {
    /// Returns the source that failed preparation.
    #[must_use]
    pub const fn script_source(&self) -> &Source {
        match self {
            Self::DiagnosticsRejected { source, .. } | Self::Compile { source, .. } => source,
        }
    }

    /// Returns diagnostics produced before preparation stopped.
    #[must_use]
    pub const fn diagnostics(&self) -> &Diagnostics {
        match self {
            Self::DiagnosticsRejected { diagnostics, .. } | Self::Compile { diagnostics, .. } => {
                diagnostics
            }
        }
    }

    /// Returns the rejecting diagnostic policy, if diagnostics stopped preparation.
    #[must_use]
    pub const fn diagnostic_policy(&self) -> Option<PrepareDiagnosticPolicy> {
        match self {
            Self::DiagnosticsRejected { policy, .. } => Some(*policy),
            Self::Compile { .. } => None,
        }
    }

    /// Returns the compiler failure, if compilation stopped preparation.
    #[must_use]
    pub const fn compile_error(&self) -> Option<&CompileError> {
        match self {
            Self::DiagnosticsRejected { .. } => None,
            Self::Compile { error, .. } => Some(error),
        }
    }
}

impl fmt::Display for PrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DiagnosticsRejected {
                source,
                diagnostics,
                policy,
            } => write!(
                formatter,
                "{} rejected by diagnostic policy '{policy}' ({} errors, {} warnings)",
                source.display_name(),
                diagnostics.error_count(),
                diagnostics.warning_count()
            ),
            Self::Compile { source, error, .. } => {
                write!(formatter, "compile {}: {error}", source.display_name())
            }
        }
    }
}

impl Error for PrepareError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DiagnosticsRejected { .. } => None,
            Self::Compile { error, .. } => Some(error),
        }
    }
}

/// Error returned while loading or executing a prepared source artifact.
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedRunError {
    /// Loading the prepared bytecode into the VM failed.
    Load(PreparedLoadError),
    /// Executing the loaded module failed.
    Exec(ExecError),
}

impl fmt::Display for PreparedRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => write!(formatter, "prepared source load failed: {error}"),
            Self::Exec(error) => write!(formatter, "prepared source execution failed: {error}"),
        }
    }
}

impl Error for PreparedRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Load(error) => Some(error),
            Self::Exec(error) => Some(error),
        }
    }
}

impl From<PreparedLoadError> for PreparedRunError {
    fn from(error: PreparedLoadError) -> Self {
        Self::Load(error)
    }
}

impl From<ExecError> for PreparedRunError {
    fn from(error: ExecError) -> Self {
        Self::Exec(error)
    }
}

// The preparation entry points live beside the pipeline they drive; the core
// surface accessors keep their own impl block in the crate root.
#[allow(clippy::multiple_inherent_impl)]
impl Surface {
    /// Checks and compiles a named source with default options.
    pub fn prepare(&self, source: Source) -> Result<PreparedSource, PrepareError> {
        self.prepare_with_options(source, PrepareOptions::default())
    }

    /// Checks and compiles a named source with explicit options.
    pub fn prepare_with_options(
        &self,
        source: Source,
        options: PrepareOptions,
    ) -> Result<PreparedSource, PrepareError> {
        let checked = self.check(
            &source,
            CheckOptions::default().with_config(options.check_config),
        );
        let diagnostics = checked.diagnostics().clone();
        if !options.diagnostic_policy.accepts(&diagnostics) {
            return Err(PrepareError::DiagnosticsRejected {
                source: Box::new(source),
                diagnostics,
                policy: options.diagnostic_policy,
            });
        }

        let chunk = self
            .compile(&source, &options.compile_options)
            .map_err(|error| PrepareError::Compile {
                source: Box::new(source.clone()),
                diagnostics: diagnostics.clone(),
                error,
            })?;
        Ok(PreparedSource {
            root: PreparedRoot {
                source,
                chunk,
                runtime_capabilities: self.runtime_capabilities().clone(),
            },
            diagnostics,
        })
    }

    /// Checks and compiles a root source and its reachable module graph.
    ///
    /// # Errors
    /// Returns [`PrepareGraphError`] when graph diagnostics fail the default
    /// policy or root compilation fails.
    pub fn prepare_graph_ready(&self, source: Source) -> Result<PreparedGraph, PrepareGraphError> {
        self.prepare_graph_ready_with_options(source, PrepareOptions::default())
    }

    /// Checks and compiles a root source and its reachable module graph with
    /// explicit preparation options.
    ///
    /// The synchronous graph check requires immediately-ready module sources.
    /// Use [`Self::prepare_graph`] for sources that may return pending
    /// futures.
    ///
    /// # Errors
    /// Returns [`PrepareGraphError`] when graph diagnostics fail the policy or
    /// root compilation fails.
    pub fn prepare_graph_ready_with_options(
        &self,
        source: Source,
        options: PrepareOptions,
    ) -> Result<PreparedGraph, PrepareGraphError> {
        let graph = self
            .check_graph_ready(
                GraphRoot::overlay(&source),
                GraphCheckOptions::default()
                    .with_parse_config(options.check_config.parse)
                    .with_mode(
                        options
                            .check_config
                            .source_mode_override
                            .unwrap_or_else(|| self.analysis_mode()),
                    )
                    .with_limits(options.graph_limits),
            )
            .map_err(|error| PrepareGraphError::GraphCheck {
                source: Box::new(source),
                error,
            })?;
        self.prepare_checked_graph(graph, options)
    }

    /// Asynchronously checks and compiles a root source and its reachable
    /// module graph with default preparation options.
    ///
    /// # Errors
    /// Returns [`PrepareGraphError`] when graph diagnostics fail the default
    /// policy or root compilation fails.
    pub async fn prepare_graph(&self, source: Source) -> Result<PreparedGraph, PrepareGraphError> {
        self.prepare_graph_with_options(source, PrepareOptions::default())
            .await
    }

    /// Asynchronously checks and compiles a root source and its reachable
    /// module graph with explicit preparation options.
    ///
    /// # Errors
    /// Returns [`PrepareGraphError`] when graph diagnostics fail the policy or
    /// root compilation fails.
    pub async fn prepare_graph_with_options(
        &self,
        source: Source,
        options: PrepareOptions,
    ) -> Result<PreparedGraph, PrepareGraphError> {
        let graph = self
            .check_graph(
                GraphRoot::overlay(&source),
                GraphCheckOptions::default()
                    .with_parse_config(options.check_config.parse)
                    .with_mode(
                        options
                            .check_config
                            .source_mode_override
                            .unwrap_or_else(|| self.analysis_mode()),
                    )
                    .with_limits(options.graph_limits),
            )
            .await
            .map_err(|error| PrepareGraphError::GraphCheck {
                source: Box::new(source),
                error,
            })?;
        self.prepare_checked_graph(graph, options)
    }

    /// Compiles a root source after a caller has completed graph checking.
    ///
    /// This is the incremental companion to [`Self::prepare_graph_async`]: a
    /// caller that records check and compile timings separately can await
    /// [`Self::check_graph_async`] and pass the resulting immutable graph here.
    /// The graph root must match the source identity.
    ///
    /// # Errors
    /// Returns [`PrepareGraphError`] when roots differ, diagnostics fail the
    /// policy, or root compilation fails.
    pub fn prepare_checked_graph(
        &self,
        graph: CheckedGraph,
        options: PrepareOptions,
    ) -> Result<PreparedGraph, PrepareGraphError> {
        let PrepareOptions {
            diagnostic_policy,
            compile_options,
            ..
        } = options;
        let source = graph
            .source_read(graph.root())
            .map(|read| read.source().clone())
            .ok_or_else(|| PrepareGraphError::MissingSourceObservation {
                module: ModuleId::from(graph.root()),
                graph: Box::new(graph.clone()),
            })?;
        Self::validate_prepared_graph(&source, &graph, diagnostic_policy)?;
        let modules = self.compile_graph_modules(&source, &graph, &compile_options)?;
        self.finish_prepared_graph(source, graph, &compile_options, modules)
    }

    fn validate_prepared_graph(
        source: &Source,
        graph: &CheckedGraph,
        diagnostic_policy: PrepareDiagnosticPolicy,
    ) -> Result<(), PrepareGraphError> {
        if !diagnostic_policy.accepts_graph(graph.diagnostics()) {
            return Err(PrepareGraphError::DiagnosticsRejected {
                source: Box::new(source.clone()),
                graph: Box::new(graph.clone()),
                policy: diagnostic_policy,
            });
        }
        Ok(())
    }

    fn compile_graph_modules(
        &self,
        source: &Source,
        graph: &CheckedGraph,
        options: &CompileOptions,
    ) -> Result<BTreeMap<ModuleId, PreparedGraphModule>, PrepareGraphError> {
        let mut modules = BTreeMap::new();
        for name in graph
            .checked_modules()
            .keys()
            .filter(|name| *name != graph.root())
        {
            let id = ModuleId::from(name);
            let read = graph.source_read(name).ok_or_else(|| {
                PrepareGraphError::MissingSourceObservation {
                    graph: Box::new(graph.clone()),
                    module: id.clone(),
                }
            })?;
            let bytes = read.source().as_bytes().to_vec();
            let chunk = self.compile(read.source(), options).map_err(|error| {
                PrepareGraphError::DependencyCompile {
                    source: Box::new(source.clone()),
                    graph: Box::new(graph.clone()),
                    module: id.clone(),
                    error,
                }
            })?;
            modules.insert(
                id,
                PreparedGraphModule {
                    source: bytes,
                    chunk,
                },
            );
        }
        Ok(modules)
    }

    fn finish_prepared_graph(
        &self,
        source: Source,
        graph: CheckedGraph,
        compile_options: &CompileOptions,
        modules: BTreeMap<ModuleId, PreparedGraphModule>,
    ) -> Result<PreparedGraph, PrepareGraphError> {
        let chunk =
            self.compile(&source, compile_options)
                .map_err(|error| PrepareGraphError::Compile {
                    source: Box::new(source.clone()),
                    graph: Box::new(graph.clone()),
                    error,
                })?;
        let module_source = self.module_source();
        let source_epoch = graph
            .source_read(graph.root())
            .map_or(0, ruau_source::SourceRead::epoch);
        Ok(PreparedGraph {
            root: PreparedRoot {
                source,
                chunk,
                runtime_capabilities: self.runtime_capabilities().clone(),
            },
            graph,
            modules: Arc::new(modules),
            module_source,
            source_epoch,
        })
    }
}
