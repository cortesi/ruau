//! Runtime and checker surface configuration.
//!
//! A [`Surface`] combines runtime capabilities, native modules, declaration
//! modules, and optional `require` source.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    sync::Arc,
};

use ruau_analysis::{
    ParseGraphResult,
    resolve::{AnalysisMode, config::EmptyResolver},
};
use ruau_ast::{
    parse::{Options, SyntaxFlags, parse_file_with},
    syntax::{Stat, TableProp, Type},
};
use ruau_bytecode::{BytecodeChunk, CompileError, CompileOptions, CompilerOptions};
use ruau_decl as decl;
use ruau_source::{ModuleName, ModuleSource, RootOverlaySource, Source};
use ruau_typecheck::{
    builtins::{BuiltinEnvironment, DefinitionModule},
    checker::{CheckedModule, Checker, Config, ConformanceCheck},
    diagnostics::{Diagnostics, GraphDiagnostics},
    frontend::GraphChecker,
    types::{Arena, TypeId},
    views::TypeView,
};
use ruau_vm::{
    Ambient, CallOptions, ExecError, HostType, Library, Limits, LoadError, LoadedModule,
    MarshaledValue, RuntimeCapabilities, Vm, VmBuilder, VmSandboxPolicy,
};
use ruau_vm_api::{
    HostFunction, ModuleBinding, ModuleBuilder, ModuleExport, ModuleValue, NativeModule,
};

static EMPTY_CONFIG_RESOLVER: EmptyResolver = EmptyResolver;

fn builtin_environment_for(
    capabilities: &RuntimeCapabilities,
    arena: &mut Arena,
) -> BuiltinEnvironment {
    builtin_environment_for_with_definition_modules(capabilities, arena, &[])
}

fn builtin_environment_for_with_definition_modules(
    capabilities: &RuntimeCapabilities,
    arena: &mut Arena,
    definition_modules: &[DefinitionModule],
) -> BuiltinEnvironment {
    let omitted_libraries = capabilities.omitted_libraries().map(Library::global_name);
    let omitted_runtime_compilation =
        (!capabilities.runtime_compilation_enabled()).then_some("loadstring");
    BuiltinEnvironment::standard_with_definition_modules(arena, definition_modules)
        .without_globals(omitted_libraries.chain(omitted_runtime_compilation))
}

/// Surface or runner configuration error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// No [`Surface`] was selected.
    MissingSurface,
    /// No [`Ambient`] was selected.
    MissingAmbient,
    /// The runner requires [`Ambient::production`].
    NonProductionAmbient,
    /// No source byte cap was set.
    MissingSourceCap,
    /// The configured source byte cap was zero.
    ZeroSourceCap,
    /// The base limits left the gas budget unbounded.
    MissingGasLimit,
    /// The configured gas budget was zero.
    ZeroGasLimit,
    /// The base limits left the memory cap unbounded.
    MissingMemoryLimit,
    /// The configured memory cap was zero.
    ZeroMemoryLimit,
    /// The configured lane count was zero.
    ZeroLaneCount,
    /// No execution feature set was selected.
    MissingFeatures,
    /// A host module declaration does not match the bindings it registers.
    InvalidHostModuleDeclaration {
        /// Stable module name.
        module: String,
        /// Human-readable validation failure.
        reason: String,
    },
    /// A required-export type expression failed to parse or to resolve
    /// against the surface's declared types.
    InvalidRequiredGlobal {
        /// Required global name.
        name: String,
        /// Human-readable validation failure.
        reason: String,
    },
    /// A declaration-only host global failed to parse or validate.
    InvalidDeclarationGlobal {
        /// Host global name.
        name: String,
        /// Human-readable validation failure.
        reason: String,
    },
    /// A compatibility feature is not supported by this runner.
    UnsupportedFeature,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ruau configuration error: ")?;
        let reason = match self {
            Self::MissingSurface => "no surface selected",
            Self::MissingAmbient => "no production ambient seam selected",
            Self::NonProductionAmbient => "production runner requires a production ambient seam",
            Self::MissingSourceCap => "no source byte cap configured",
            Self::ZeroSourceCap => "source byte cap is zero",
            Self::MissingGasLimit => "limits left the gas budget unbounded",
            Self::ZeroGasLimit => "gas budget is zero",
            Self::MissingMemoryLimit => "limits left the memory cap unbounded",
            Self::ZeroMemoryLimit => "memory cap is zero",
            Self::ZeroLaneCount => "lane count is zero",
            Self::MissingFeatures => "no execution feature set selected",
            Self::InvalidHostModuleDeclaration { module, reason } => {
                return write!(f, "host module {module} declaration is invalid: {reason}");
            }
            Self::InvalidRequiredGlobal { name, reason } => {
                return write!(f, "required global {name} is invalid: {reason}");
            }
            Self::InvalidDeclarationGlobal { name, reason } => {
                return write!(f, "declaration global {name} is invalid: {reason}");
            }
            Self::UnsupportedFeature => {
                "a compatibility feature is enabled but not yet wired into the pipeline"
            }
        };
        f.write_str(reason)
    }
}

impl std::error::Error for ConfigError {}

/// Surface graph-checking error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphCheckError {
    /// The surface has no module source for an existing-module graph check.
    MissingModuleSource,
}

impl fmt::Display for GraphCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingModuleSource => formatter.write_str(
                "surface graph check requires a module source or a synthetic root source",
            ),
        }
    }
}

impl Error for GraphCheckError {}

/// Error returned while loading or executing a prepared source artifact.
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedRunError {
    /// Loading the prepared bytecode into the VM failed.
    Load(LoadError),
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

impl From<LoadError> for PreparedRunError {
    fn from(error: LoadError) -> Self {
        Self::Load(error)
    }
}

impl From<ExecError> for PreparedRunError {
    fn from(error: ExecError) -> Self {
        Self::Exec(error)
    }
}

/// Named VM execution policy for a [`Surface`]-built VM.
///
/// This groups the construction-time ambient environment, VM default limits,
/// and sandbox policy that used to be passed partly as positional arguments
/// and partly as builder calls.
#[derive(Clone, Debug)]
pub struct VmConfig {
    ambient: Ambient,
    limits: Limits,
    sandbox_policy: VmSandboxPolicy,
}

impl VmConfig {
    /// Builds an untrusted-code VM configuration from explicit ambient and
    /// limit values.
    #[must_use]
    pub fn untrusted(ambient: Ambient, limits: Limits) -> Self {
        Self {
            ambient,
            limits,
            sandbox_policy: VmSandboxPolicy::Untrusted,
        }
    }

    /// Builds a deterministic, sandboxed configuration for tests and examples
    /// that set their own limits per call.
    #[must_use]
    pub fn deterministic(seed: u64) -> Self {
        Self::untrusted(Ambient::deterministic(seed), Limits::unlimited())
    }

    /// Builds a deterministic, sandboxed configuration with production-style
    /// gas, heap, string, buffer, table, pack, and runtime-compile caps.
    #[must_use]
    pub fn metered_untrusted(seed: u64, gas: u64, max_memory_bytes: usize) -> Self {
        Self::untrusted(
            Ambient::deterministic(seed),
            Limits::production(gas, max_memory_bytes),
        )
    }

    /// Builds a production-ambient, sandboxed configuration with
    /// production-style limits.
    #[must_use]
    pub fn production(seed: u64, gas: u64, max_memory_bytes: usize) -> Self {
        Self::untrusted(
            Ambient::production(seed),
            Limits::production(gas, max_memory_bytes),
        )
    }

    /// Builds a trusted host/internal VM configuration without installing the
    /// untrusted-code sandbox.
    #[must_use]
    pub fn trusted_host(ambient: Ambient, limits: Limits) -> Self {
        Self {
            ambient,
            limits,
            sandbox_policy: VmSandboxPolicy::TrustedHost,
        }
    }

    /// Returns the ambient environment selected for the VM.
    #[must_use]
    pub const fn ambient(&self) -> Ambient {
        self.ambient
    }

    /// Returns the VM default limits.
    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Returns the sandbox policy applied during VM construction.
    #[must_use]
    pub const fn sandbox_policy(&self) -> VmSandboxPolicy {
        self.sandbox_policy
    }

    /// Replaces the ambient environment.
    #[must_use]
    pub fn with_ambient(mut self, ambient: Ambient) -> Self {
        self.ambient = ambient;
        self
    }

    /// Replaces the VM default limits.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Selects the untrusted-code sandbox.
    #[must_use]
    pub fn sandboxed(mut self) -> Self {
        self.sandbox_policy = VmSandboxPolicy::Untrusted;
        self
    }

    /// Selects trusted host/internal execution without the untrusted-code
    /// sandbox.
    #[must_use]
    pub fn trusted(mut self) -> Self {
        self.sandbox_policy = VmSandboxPolicy::TrustedHost;
        self
    }
}

/// Result of checking a source graph through a [`Surface`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckedGraph {
    result: ParseGraphResult,
    diagnostics: GraphDiagnostics,
    checked_modules: BTreeMap<ModuleName, CheckedModule>,
}

impl CheckedGraph {
    fn from_frontend(frontend: &GraphChecker<'_>, result: ParseGraphResult) -> Self {
        let diagnostics = frontend.graph_diagnostics(&result);
        let checked_modules = frontend.checked_modules().clone();
        Self {
            result,
            diagnostics,
            checked_modules,
        }
    }

    /// Returns the parsed graph result.
    #[must_use]
    pub const fn result(&self) -> &ParseGraphResult {
        &self.result
    }

    /// Returns the requested root module.
    #[must_use]
    pub const fn root(&self) -> &ModuleName {
        &self.result.root
    }

    /// Returns dependency-first modules reached by the graph check.
    #[must_use]
    pub fn build_queue(&self) -> &[ModuleName] {
        &self.result.build_queue
    }

    /// Returns whether the parsed graph contains a require cycle.
    #[must_use]
    pub const fn cycle_detected(&self) -> bool {
        self.result.cycle_detected
    }

    /// Returns module-qualified diagnostics with display names preserved.
    #[must_use]
    pub const fn diagnostics(&self) -> &GraphDiagnostics {
        &self.diagnostics
    }

    /// Returns true when any graph diagnostic is present.
    #[must_use]
    pub fn has_issues(&self) -> bool {
        self.diagnostics.has_issues()
    }

    /// Returns true when any error-severity graph diagnostic is present.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.has_errors()
    }

    /// Returns one checked module by name.
    #[must_use]
    pub fn checked_module(&self, name: &ModuleName) -> Option<&CheckedModule> {
        self.checked_modules.get(name)
    }

    /// Returns all checked modules keyed by module name.
    #[must_use]
    pub const fn checked_modules(&self) -> &BTreeMap<ModuleName, CheckedModule> {
        &self.checked_modules
    }

    /// Consumes the graph result and returns its parts.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        ParseGraphResult,
        GraphDiagnostics,
        BTreeMap<ModuleName, CheckedModule>,
    ) {
        (self.result, self.diagnostics, self.checked_modules)
    }
}

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
    /// Default preparation policy: reject error-severity diagnostics.
    #[must_use]
    pub const fn reject_errors() -> Self {
        Self::RejectErrors
    }

    /// Stricter preparation policy: reject warnings as well as errors.
    #[must_use]
    pub const fn reject_issues() -> Self {
        Self::RejectIssues
    }

    /// Advanced preparation policy: keep diagnostics but continue to compile.
    #[must_use]
    pub const fn allow_diagnostics() -> Self {
        Self::AllowDiagnostics
    }

    fn accepts(self, diagnostics: &Diagnostics) -> bool {
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
}

/// A checked and compiled source artifact ready to load into a matching VM.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedScript {
    source: Source,
    diagnostics: Diagnostics,
    chunk: BytecodeChunk,
    runtime_capabilities: RuntimeCapabilities,
}

impl PreparedScript {
    /// Returns the source identity and bytes used for checking and compilation.
    #[must_use]
    pub const fn source(&self) -> &Source {
        &self.source
    }

    /// Returns diagnostics produced during checking.
    #[must_use]
    pub const fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// Returns the compiled bytecode chunk.
    #[must_use]
    pub const fn chunk(&self) -> &BytecodeChunk {
        &self.chunk
    }

    /// Returns the runtime capabilities used for compilation.
    #[must_use]
    pub const fn runtime_capabilities(&self) -> &RuntimeCapabilities {
        &self.runtime_capabilities
    }

    /// Returns the Lua chunk name bytes for loading this script.
    #[must_use]
    pub fn load_name(&self) -> Vec<u8> {
        self.source.load_name()
    }

    /// Loads this prepared source into `vm`, preserving both its traceback
    /// load name and its module requester identity.
    ///
    /// # Errors
    /// Returns [`LoadError`] when the prepared chunk cannot be instantiated in
    /// the VM.
    pub fn load_in(&self, vm: &mut Vm) -> Result<LoadedModule, LoadError> {
        let load_name = self.source.load_name();
        vm.load_named_module(&self.chunk, self.source.id().clone(), &load_name)
    }

    /// Loads and executes this prepared source in `vm` with empty call options.
    ///
    /// # Errors
    /// Returns [`PreparedRunError`] when loading or execution fails.
    pub fn run_in(&self, vm: &mut Vm) -> Result<Vec<MarshaledValue>, PreparedRunError> {
        self.run_in_with_options(vm, CallOptions::new())
    }

    /// Loads and executes this prepared source in `vm` with explicit call
    /// options.
    ///
    /// # Errors
    /// Returns [`PreparedRunError`] when loading or execution fails.
    pub fn run_in_with_options(
        &self,
        vm: &mut Vm,
        options: CallOptions,
    ) -> Result<Vec<MarshaledValue>, PreparedRunError> {
        let module = self.load_in(vm).map_err(PreparedRunError::Load)?;
        let result = vm.exec(&module, options).map_err(PreparedRunError::Exec);
        vm.unload(module);
        result
    }

    /// Consumes the artifact and returns its parts.
    #[must_use]
    pub fn into_parts(self) -> (Source, Diagnostics, BytecodeChunk, RuntimeCapabilities) {
        (
            self.source,
            self.diagnostics,
            self.chunk,
            self.runtime_capabilities,
        )
    }

    /// Consumes the artifact and returns its compiled bytecode chunk.
    #[must_use]
    pub fn into_chunk(self) -> BytecodeChunk {
        self.chunk
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
    pub const fn source(&self) -> &Source {
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

/// A validated runtime and checker surface.
#[derive(Clone)]
pub struct Surface {
    runtime_capabilities: RuntimeCapabilities,
    analysis_mode: AnalysisMode,
    modules: Vec<Arc<dyn NativeModule>>,
    module_declarations: Vec<DefinitionModule>,
    host_module_manifest_version: u64,
    module_source: Option<Arc<dyn ModuleSource>>,
    declaration_globals: Vec<DeclarationGlobalSpec>,
    /// Lazily-built checker base shared by every clone of this surface: the
    /// builtin type environment, native require returns, and arena are
    /// constructed once and forked (cloned) per request, instead of rebuilt
    /// from declarations each time.
    checker_base: Arc<std::sync::OnceLock<SurfaceCheckerBase>>,
    /// Required exports replayed onto every [`Self::new_checker`] checker,
    /// validated against this surface's declared types at registration.
    required_globals: Vec<RequiredGlobalSpec>,
}

/// One validated required-export obligation carried by a [`Surface`].
#[derive(Clone, Debug)]
struct RequiredGlobalSpec {
    name: String,
    type_text: String,
}

#[derive(Clone, Debug)]
struct DeclarationGlobalSpec {
    name: String,
    type_text: String,
}

#[derive(Clone)]
struct SurfaceCheckerBase {
    arena: Arena,
    builtins: BuiltinEnvironment,
    ambient_require_returns: Vec<(String, TypeId)>,
}

impl DeclarationGlobalSpec {
    fn source(&self) -> String {
        format!("declare {}: {}", self.name, self.type_text)
    }

    fn definition_module(&self) -> DefinitionModule {
        DefinitionModule {
            name: format!("<host-global:{}>", self.name).into(),
            source: self.source().into(),
        }
    }
}

impl Surface {
    /// Builds the default safe surface: all standard libraries, strict
    /// analysis, no host modules, no module source, and no runtime source
    /// compilation.
    #[must_use]
    pub fn new() -> Self {
        Self::builder()
            .build()
            .expect("the default surface configuration is valid")
    }

    /// Starts a surface builder with the default safe surface: all standard
    /// libraries, strict analysis, no host modules, no module source, and no
    /// runtime source compilation.
    #[must_use]
    pub fn builder() -> SurfaceBuilder {
        SurfaceBuilder {
            libraries: Library::ALL.to_vec(),
            runtime_compilation: false,
            analysis_mode: AnalysisMode::Strict,
            modules: Vec::new(),
            module_source: None,
            declaration_globals: Vec::new(),
            required_globals: Vec::new(),
        }
    }

    fn from_validated_parts(
        runtime_capabilities: RuntimeCapabilities,
        analysis_mode: AnalysisMode,
        modules: Vec<Arc<dyn NativeModule>>,
        module_source: Option<Arc<dyn ModuleSource>>,
        mut module_declarations: Vec<DefinitionModule>,
        declaration_globals: Vec<DeclarationGlobalSpec>,
    ) -> Result<Self, ConfigError> {
        for global in &declaration_globals {
            module_declarations.push(global.definition_module());
        }
        let host_module_manifest_version =
            host_module_manifest_version(&modules, &module_declarations);
        Ok(Self {
            runtime_capabilities,
            analysis_mode,
            modules,
            module_declarations,
            host_module_manifest_version,
            module_source,
            checker_base: Arc::new(std::sync::OnceLock::new()),
            required_globals: Vec::new(),
            declaration_globals,
        })
    }

    /// The lower-level VM runtime identity for this surface.
    #[must_use]
    pub fn runtime_capabilities(&self) -> &RuntimeCapabilities {
        &self.runtime_capabilities
    }

    /// The standard libraries selected by this surface.
    #[must_use]
    pub fn libraries(&self) -> &[Library] {
        self.runtime_capabilities.libraries()
    }

    /// Whether this surface grants runtime source compilation through `loadstring`.
    #[must_use]
    pub fn runtime_compilation_enabled(&self) -> bool {
        self.runtime_capabilities.runtime_compilation_enabled()
    }

    /// Analysis mode used by checker helpers.
    #[must_use]
    pub const fn analysis_mode(&self) -> AnalysisMode {
        self.analysis_mode
    }

    /// Audited host modules installed into each request VM.
    #[must_use]
    pub fn native_modules(&self) -> &[Arc<dyn NativeModule>] {
        &self.modules
    }

    /// Declaration modules used by the checker for the native-module surface.
    #[must_use]
    pub fn declaration_modules(&self) -> &[DefinitionModule] {
        &self.module_declarations
    }

    /// Stable hash of the configured host-module declaration manifest.
    #[must_use]
    pub fn host_module_manifest_version(&self) -> u64 {
        self.host_module_manifest_version
    }

    /// Whether this surface grants `require` by installing a module source.
    #[must_use]
    pub fn has_module_source(&self) -> bool {
        self.module_source.is_some()
    }

    /// Builds the type-checker builtin environment for this surface.
    #[must_use]
    pub fn builtin_environment(&self, arena: &mut Arena) -> BuiltinEnvironment {
        self.builtin_environment_with_require_returns(arena).0
    }

    fn checker_base(&self) -> SurfaceCheckerBase {
        let mut arena = Arena::new();
        let (builtins, ambient_require_returns) =
            self.builtin_environment_with_require_returns(&mut arena);
        SurfaceCheckerBase {
            arena,
            builtins,
            ambient_require_returns,
        }
    }

    fn builtin_environment_with_require_returns(
        &self,
        arena: &mut Arena,
    ) -> (BuiltinEnvironment, Vec<(String, TypeId)>) {
        let mut builtins = builtin_environment_for_with_definition_modules(
            self.runtime_capabilities(),
            arena,
            self.declaration_modules(),
        );
        let ambient_require_returns = self
            .modules
            .iter()
            .filter(|module| !matches!(module.export(), ModuleExport::Globals))
            .filter_map(|module| {
                builtins
                    .global(module.name())
                    .map(|global| (module.name().to_owned(), global.ty))
            })
            .collect::<Vec<_>>();
        if !self.has_module_source() && ambient_require_returns.is_empty() {
            builtins = builtins.without_globals(["require"]);
        }
        let require_only_globals = self
            .modules
            .iter()
            .filter(|module| matches!(module.export(), ModuleExport::Require))
            .map(|module| module.name())
            .collect::<Vec<_>>();
        builtins = builtins.without_globals(require_only_globals);
        (builtins, ambient_require_returns)
    }

    /// Builds a checker session for this surface.
    #[must_use]
    pub fn new_checker(&self) -> Checker {
        let base = self.checker_base.get_or_init(|| self.checker_base());
        let mut checker = Checker::with_builtins(base.arena.clone(), base.builtins.clone());
        for (module, ty) in &base.ambient_require_returns {
            checker.define_require_return(module.as_str(), *ty);
        }
        for required in &self.required_globals {
            checker
                .require_global(&required.name, &required.type_text)
                .expect("required globals are validated when registered");
        }
        checker
    }

    /// Checks source text using this surface's environment and analysis mode.
    #[must_use]
    pub fn check_source(&self, source: &str) -> CheckedModule {
        self.check_source_with_config(source, Config::default())
    }

    /// Checks source text using this surface's builtin environment and an
    /// explicit checker configuration.
    ///
    /// If `config` does not already force a source mode, this method fills the
    /// override from [`Self::analysis_mode`]. Caller-provided overrides win.
    #[must_use]
    pub fn check_source_with_config(&self, source: &str, config: Config) -> CheckedModule {
        let mut checker = self.new_checker();
        checker.check_source_with_config(source, self.surface_config(config))
    }

    /// Checks arbitrary source bytes using this surface's environment and
    /// analysis mode.
    #[must_use]
    pub fn check_source_bytes(&self, source: &[u8]) -> CheckedModule {
        self.check_source_bytes_with_config(source, Config::default())
    }

    /// Checks arbitrary source bytes using this surface's builtin environment
    /// and an explicit checker configuration.
    ///
    /// If `config` does not already force a source mode, this method fills the
    /// override from [`Self::analysis_mode`]. Caller-provided overrides win.
    #[must_use]
    pub fn check_source_bytes_with_config(&self, source: &[u8], config: Config) -> CheckedModule {
        let mut checker = self.new_checker();
        checker.check_source_bytes_with_config(source, self.surface_config(config))
    }

    /// Checks a named source using this surface's environment and analysis mode.
    #[must_use]
    pub fn check(&self, source: &Source) -> CheckedModule {
        self.check_with_config(source, Config::default())
    }

    /// Checks a named source using this surface's builtin environment and
    /// an explicit checker configuration.
    ///
    /// UTF-8 sources use the text checker path; byte-exact sources with
    /// invalid UTF-8 use the byte checker path.
    #[must_use]
    pub fn check_with_config(&self, source: &Source, config: Config) -> CheckedModule {
        if let Some(text) = source.source_str() {
            self.check_source_with_config(text, config)
        } else {
            self.check_source_bytes_with_config(source.source(), config)
        }
    }

    /// Checks an existing module-source root and its statically reachable graph.
    ///
    /// This ready-only bridge reports pending async source futures as resolver
    /// diagnostics in the returned graph. Use [`Self::check_module_graph_async`]
    /// to await async source reads and resolutions.
    ///
    /// # Errors
    /// Returns [`GraphCheckError::MissingModuleSource`] when this surface has no
    /// module source installed.
    pub fn check_module_graph(
        &self,
        root: impl Into<ModuleName>,
    ) -> Result<CheckedGraph, GraphCheckError> {
        let Some(source) = self.module_source() else {
            return Err(GraphCheckError::MissingModuleSource);
        };
        let mut frontend = self.graph_checker(source.as_ref());
        let result = frontend.check(root);
        Ok(CheckedGraph::from_frontend(&frontend, result))
    }

    /// Checks an existing module-source root and awaits async source futures.
    ///
    /// # Errors
    /// Returns [`GraphCheckError::MissingModuleSource`] when this surface has no
    /// module source installed.
    pub async fn check_module_graph_async(
        &self,
        root: impl Into<ModuleName>,
    ) -> Result<CheckedGraph, GraphCheckError> {
        let Some(source) = self.module_source() else {
            return Err(GraphCheckError::MissingModuleSource);
        };
        let mut frontend = self.graph_checker(source.as_ref());
        let result = frontend.check_async(root).await;
        Ok(CheckedGraph::from_frontend(&frontend, result))
    }

    /// Checks a synthetic root source plus dependencies from this surface's
    /// optional module source.
    ///
    /// This ready-only bridge reports pending async source futures as resolver
    /// diagnostics in the returned graph. Use [`Self::check_source_graph_async`]
    /// to await async source reads and resolutions.
    #[must_use]
    pub fn check_source_graph(&self, source: &Source) -> CheckedGraph {
        let source = self.root_overlay_source(source);
        let root = source.root_name();
        let mut frontend = self.graph_checker(&source);
        let result = frontend.check(root);
        CheckedGraph::from_frontend(&frontend, result)
    }

    /// Checks a synthetic root source plus dependencies from this surface's
    /// optional module source, awaiting async source futures.
    pub async fn check_source_graph_async(&self, source: &Source) -> CheckedGraph {
        let source = self.root_overlay_source(source);
        let root = source.root_name();
        let mut frontend = self.graph_checker(&source);
        let result = frontend.check_async(root).await;
        CheckedGraph::from_frontend(&frontend, result)
    }

    fn graph_checker<'source>(&self, source: &'source dyn ModuleSource) -> GraphChecker<'source> {
        let mut frontend =
            GraphChecker::with_checker(source, &EMPTY_CONFIG_RESOLVER, self.new_checker());
        frontend.set_source_mode_override(Some(self.analysis_mode()));
        frontend
    }

    fn root_overlay_source(&self, source: &Source) -> RootOverlaySource<'static> {
        let mut overlay = RootOverlaySource::new(source.id().clone(), source.source().to_vec())
            .with_display_name(source.display_name().to_owned())
            .with_root_requester(source.id().clone())
            .reject_delegate_root_id_collision(true);
        if let Some(module_source) = self.module_source() {
            overlay = overlay.with_owned_delegate(module_source);
        }
        overlay
    }

    /// Checks and compiles a named source with default preparation options.
    ///
    /// The default diagnostic policy rejects error-severity diagnostics,
    /// preserves warnings on the returned artifact, and compiles with the
    /// public VM compile policy.
    ///
    /// # Errors
    /// Returns [`PrepareError`] when diagnostics fail the policy or compilation
    /// fails after diagnostics are accepted.
    pub fn prepare(&self, source: Source) -> Result<PreparedScript, PrepareError> {
        self.prepare_with_options(source, PrepareOptions::default())
    }

    /// Checks and compiles a named source with explicit preparation options.
    ///
    /// # Errors
    /// Returns [`PrepareError`] when diagnostics fail the policy or compilation
    /// fails after diagnostics are accepted.
    pub fn prepare_with_options(
        &self,
        source: Source,
        options: PrepareOptions,
    ) -> Result<PreparedScript, PrepareError> {
        let checked = self.check_with_config(&source, options.check_config);
        let diagnostics = checked.diagnostics().clone();
        if !options.diagnostic_policy.accepts(&diagnostics) {
            return Err(PrepareError::DiagnosticsRejected {
                source: Box::new(source),
                diagnostics,
                policy: options.diagnostic_policy,
            });
        }

        let chunk = self
            .compile_source_with_options(&source, &options.compile_options)
            .map_err(|error| PrepareError::Compile {
                source: Box::new(source.clone()),
                diagnostics: diagnostics.clone(),
                error,
            })?;
        Ok(PreparedScript {
            source,
            diagnostics,
            chunk,
            runtime_capabilities: self.runtime_capabilities().clone(),
        })
    }

    fn surface_config(&self, mut config: Config) -> Config {
        if config.source_mode_override.is_none() {
            config.source_mode_override = Some(self.analysis_mode());
            config.default_mode = self.analysis_mode();
        }
        config
    }

    /// Checks an implementation source against a declaration.
    #[must_use]
    pub fn check_conformance(
        &self,
        implementation_source: &str,
        declaration_source: &str,
    ) -> ConformanceCheck {
        let mut checker = self.new_checker();
        checker.check_conformance_report_with_config(
            implementation_source,
            declaration_source,
            Config::with_source_mode(self.analysis_mode),
        )
    }

    /// Requires checked modules to define global `name` as `type_text`.
    ///
    /// `type_text` is the type portion of `declare name: <type>`.
    ///
    /// # Errors
    /// Returns [`ConfigError::InvalidRequiredGlobal`] when `type_text`
    /// does not parse or references type names the surface does not declare.
    pub fn require_global(&mut self, name: &str, type_text: &str) -> Result<(), ConfigError> {
        let mut probe = self.new_checker();
        probe
            .require_global(name, type_text)
            .map_err(|diagnostics| ConfigError::InvalidRequiredGlobal {
                name: name.to_owned(),
                reason: diagnostics.render("<required-export>"),
            })?;
        self.required_globals.push(RequiredGlobalSpec {
            name: name.to_owned(),
            type_text: type_text.to_owned(),
        });
        Ok(())
    }

    /// Returns a [`VmBuilder`] configured with this surface's runtime
    /// capabilities, native modules, optional `require` source, and VM
    /// execution policy.
    #[must_use]
    pub fn vm_builder(&self, config: &VmConfig) -> VmBuilder {
        let mut builder = ruau_vm::Vm::builder()
            .ambient(config.ambient())
            .limits(config.limits().clone())
            .runtime_capabilities(self.runtime_capabilities().clone());
        builder = match config.sandbox_policy() {
            VmSandboxPolicy::TrustedHost => builder.trusted_host(),
            VmSandboxPolicy::Untrusted => builder.sandboxed(),
        };
        if let Some(source) = self.module_source() {
            builder = builder.module_source(source);
        }
        for module in self.native_modules() {
            builder = builder.module(Arc::clone(module));
        }
        builder
    }

    /// Compiles `source` under this surface's runtime capabilities.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile(&self, source: &[u8]) -> Result<BytecodeChunk, CompileError> {
        self.compile_with_options(source, &CompileOptions::default())
    }

    /// Compiles `source` under this surface's runtime capabilities with an
    /// explicit public compile policy.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_with_options(
        &self,
        source: &[u8],
        base: &CompileOptions,
    ) -> Result<BytecodeChunk, CompileError> {
        self.runtime_capabilities().compile_source(source, base)
    }

    /// Compiles a named source under this surface's runtime capabilities.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_source(&self, source: &Source) -> Result<BytecodeChunk, CompileError> {
        self.compile_source_with_options(source, &CompileOptions::default())
    }

    /// Compiles a named source under this surface's runtime capabilities with
    /// an explicit public compile policy.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_source_with_options(
        &self,
        source: &Source,
        base: &CompileOptions,
    ) -> Result<BytecodeChunk, CompileError> {
        self.compile_with_options(source.source(), base)
    }

    /// Compiles `source` with the repository's upstream-fixture option shape.
    #[doc(hidden)]
    pub fn compile_with_compiler_options(
        &self,
        source: &[u8],
        base: &CompilerOptions,
    ) -> Result<BytecodeChunk, CompileError> {
        self.runtime_capabilities()
            .compile_source_with_compiler_options(source, base)
    }

    /// Compiles and validates `source` into a [`CompiledModule`](ruau_vm::CompiledModule).
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_module(&self, source: &[u8]) -> Result<ruau_vm::CompiledModule, CompileError> {
        self.compile_module_with_options(source, &CompileOptions::default())
    }

    /// Compiles and validates `source` with an explicit public compile policy.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_module_with_options(
        &self,
        source: &[u8],
        base: &CompileOptions,
    ) -> Result<ruau_vm::CompiledModule, CompileError> {
        self.runtime_capabilities().compile_module(source, base)
    }

    /// Compiles and validates a named source into a
    /// [`CompiledModule`](ruau_vm::CompiledModule).
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_module_source(
        &self,
        source: &Source,
    ) -> Result<ruau_vm::CompiledModule, CompileError> {
        self.compile_module_source_with_options(source, &CompileOptions::default())
    }

    /// Compiles and validates a named source with an explicit public compile
    /// policy.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_module_source_with_options(
        &self,
        source: &Source,
        base: &CompileOptions,
    ) -> Result<ruau_vm::CompiledModule, CompileError> {
        self.compile_module_with_options(source.source(), base)
    }

    /// Compiles and validates `source` with the upstream-fixture option shape.
    #[doc(hidden)]
    pub fn compile_module_with_compiler_options(
        &self,
        source: &[u8],
        base: &CompilerOptions,
    ) -> Result<ruau_vm::CompiledModule, CompileError> {
        self.runtime_capabilities()
            .compile_module_with_compiler_options(source, base)
    }

    /// The optional `require` source this surface grants.
    #[must_use]
    pub fn module_source(&self) -> Option<Arc<dyn ModuleSource>> {
        self.module_source.clone()
    }

    /// Returns this surface with `source` installed as its runtime `require`
    /// source.
    #[must_use]
    pub fn with_module_source(mut self, source: Arc<dyn ModuleSource>) -> Self {
        self.replace_module_source(Some(source));
        self
    }

    /// Returns this surface with runtime `require` disabled.
    #[must_use]
    pub fn without_module_source(mut self) -> Self {
        self.replace_module_source(None);
        self
    }

    /// Replaces the runtime `require` source for this already-built surface.
    ///
    /// Passing `None` disables runtime `require`.
    pub fn replace_module_source(&mut self, source: Option<Arc<dyn ModuleSource>>) {
        self.module_source = source;
        self.reset_derived_state();
    }

    fn reset_derived_state(&mut self) {
        self.checker_base = Arc::new(std::sync::OnceLock::new());
    }
}

impl Default for Surface {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface")
            .field("runtime_capabilities", &self.runtime_capabilities)
            .field("analysis_mode", &self.analysis_mode)
            .field("native_modules", &self.modules.len())
            .field("declaration_modules", &self.module_declarations.len())
            .field(
                "host_module_manifest_version",
                &self.host_module_manifest_version,
            )
            .field("has_module_source", &self.module_source.is_some())
            .field("required_globals", &self.required_globals)
            .field("declaration_globals", &self.declaration_globals)
            .finish()
    }
}

/// Builder for a [`Surface`].
pub struct SurfaceBuilder {
    libraries: Vec<Library>,
    runtime_compilation: bool,
    analysis_mode: AnalysisMode,
    modules: Vec<Arc<dyn NativeModule>>,
    module_source: Option<Arc<dyn ModuleSource>>,
    declaration_globals: Vec<DeclarationGlobalSpec>,
    required_globals: Vec<RequiredGlobalSpec>,
}

impl SurfaceBuilder {
    /// Replaces the selected standard-library set exactly.
    ///
    /// Duplicates and order are normalized when the surface is built. Passing an
    /// empty iterable yields base globals only.
    #[must_use]
    pub fn libraries<I>(mut self, libraries: I) -> Self
    where
        I: IntoIterator<Item = Library>,
    {
        self.libraries = libraries.into_iter().collect();
        self
    }

    /// Enables runtime source compilation through `loadstring`.
    #[must_use]
    pub fn enable_runtime_compilation(mut self) -> Self {
        self.runtime_compilation = true;
        self
    }

    /// Selects the analysis mode used by checker helpers.
    #[must_use]
    pub fn analysis_mode(mut self, mode: AnalysisMode) -> Self {
        self.analysis_mode = mode;
        self
    }

    /// Grants one audited native module to the surface.
    #[must_use]
    pub fn module(mut self, module: Arc<dyn NativeModule>) -> Self {
        self.modules.push(module);
        self
    }

    /// Grants runtime `require` through the supplied module source.
    #[must_use]
    pub fn module_source(mut self, source: Arc<dyn ModuleSource>) -> Self {
        self.module_source = Some(source);
        self
    }

    /// Declares a checker-visible global without installing a VM value.
    #[must_use]
    pub fn declaration_global(mut self, name: &str, type_text: &str) -> Self {
        self.declaration_globals.push(DeclarationGlobalSpec {
            name: name.to_owned(),
            type_text: type_text.to_owned(),
        });
        self
    }

    /// Declares a checker-visible global from a Luau declaration type.
    #[must_use]
    pub fn declaration_global_ty(mut self, name: &str, ty: &decl::Ty) -> Self {
        self.declaration_globals.push(DeclarationGlobalSpec {
            name: name.to_owned(),
            type_text: ty.render(),
        });
        self
    }

    /// Requires checked root modules to define global `name` as `type_text`.
    ///
    /// `type_text` is the type portion of `declare name: <type>`.
    #[must_use]
    pub fn require_global(mut self, name: &str, type_text: &str) -> Self {
        self.required_globals.push(RequiredGlobalSpec {
            name: name.to_owned(),
            type_text: type_text.to_owned(),
        });
        self
    }

    /// Validates module declarations and returns the exact surface.
    ///
    /// # Errors
    /// Returns [`ConfigError`] if any host module declaration is malformed,
    /// mismatched, or tries to bind a surface-omitted library.
    pub fn build(self) -> Result<Surface, ConfigError> {
        let runtime_capabilities = if self.runtime_compilation {
            RuntimeCapabilities::from_libraries(self.libraries).enable_runtime_compilation()
        } else {
            RuntimeCapabilities::from_libraries(self.libraries)
        };
        let module_declarations = validate_host_modules(&runtime_capabilities, &self.modules)?;
        validate_declaration_globals(
            &runtime_capabilities,
            &module_declarations,
            &self.declaration_globals,
        )?;
        let mut surface = Surface::from_validated_parts(
            runtime_capabilities,
            self.analysis_mode,
            self.modules,
            self.module_source,
            module_declarations,
            self.declaration_globals,
        )?;
        for required in self.required_globals {
            surface.require_global(&required.name, &required.type_text)?;
        }
        Ok(surface)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostBindingKind {
    Function,
    Value,
    Table,
}

#[derive(Debug, Default)]
pub(crate) struct HostModuleShape {
    globals: BTreeMap<String, HostBindingKind>,
    libraries: BTreeMap<String, BTreeMap<String, HostBindingKind>>,
    library_roots: BTreeSet<String>,
    /// Script-visible module tables returned by native `require` exports.
    /// `Require` modules populate this without also installing a global;
    /// `Both` modules populate it alongside their ordinary bindings.
    module_exports: BTreeMap<String, BTreeMap<String, HostBindingKind>>,
    /// Globals registered with `ModuleBinding::GlobalOverride` — the explicit
    /// builtin-replacement opt-in. A subset of `globals` keys.
    overrides: BTreeSet<String>,
    /// Host-only (`ModuleBinding::Hidden`) tables and their members. These are
    /// never script-visible, so they take no part in the declared-shape match:
    /// the declaration must not declare a global for them, and contributes
    /// only types (aliases/classes) on their behalf.
    hidden: BTreeMap<String, BTreeMap<String, HostBindingKind>>,
    host_types: BTreeSet<String>,
    support_chunks: BTreeSet<String>,
}

impl HostModuleShape {
    fn insert_global(
        &mut self,
        module: &str,
        name: &str,
        kind: HostBindingKind,
    ) -> Result<(), String> {
        match self.globals.insert(name.to_owned(), kind) {
            None => Ok(()),
            Some(previous)
                if previous == HostBindingKind::Table
                    && kind == HostBindingKind::Table
                    && self.library_roots.contains(name) =>
            {
                self.globals.insert(name.to_owned(), previous);
                Ok(())
            }
            Some(previous) => {
                self.globals.insert(name.to_owned(), previous);
                Err(format!("module {module} declares duplicate global {name}"))
            }
        }
    }

    fn ensure_library_root(&mut self, module: &str, library: &str) -> Result<(), String> {
        match self.globals.get(library).copied() {
            None => {
                self.globals
                    .insert(library.to_owned(), HostBindingKind::Table);
                self.library_roots.insert(library.to_owned());
                Ok(())
            }
            Some(HostBindingKind::Table) if self.library_roots.contains(library) => Ok(()),
            Some(HostBindingKind::Table) => Err(format!(
                "module {module} binds library root {library} over a table global"
            )),
            Some(_) => Err(format!(
                "module {module} binds library root {library} over a non-table global"
            )),
        }
    }

    fn insert_library_member(
        &mut self,
        module: &str,
        library: &str,
        name: &str,
        kind: HostBindingKind,
    ) -> Result<(), String> {
        let members = self.libraries.entry(library.to_owned()).or_default();
        match members.insert(name.to_owned(), kind) {
            None => Ok(()),
            Some(previous) => {
                members.insert(name.to_owned(), previous);
                Err(format!(
                    "module {module} declares duplicate library binding {library}.{name}"
                ))
            }
        }
    }

    fn insert_module_export_member(
        &mut self,
        module: &str,
        name: &str,
        kind: HostBindingKind,
    ) -> Result<(), String> {
        let members = self.module_exports.entry(module.to_owned()).or_default();
        match members.insert(name.to_owned(), kind) {
            None => Ok(()),
            Some(previous) => {
                members.insert(name.to_owned(), previous);
                Err(format!(
                    "module {module} declares duplicate require export {module}.{name}"
                ))
            }
        }
    }

    fn insert_hidden_member(
        &mut self,
        module: &str,
        table: &str,
        name: &str,
        kind: HostBindingKind,
    ) -> Result<(), String> {
        if self.support_chunks.contains(table) {
            return Err(format!(
                "module {module} binds hidden table {table} over a support chunk"
            ));
        }
        let members = self.hidden.entry(table.to_owned()).or_default();
        match members.insert(name.to_owned(), kind) {
            None => Ok(()),
            Some(previous) => {
                members.insert(name.to_owned(), previous);
                Err(format!(
                    "module {module} registers duplicate hidden binding {table}.{name}"
                ))
            }
        }
    }

    fn insert_support_chunk(&mut self, module: &str, key: &str) -> Result<(), String> {
        if self.hidden.contains_key(key) {
            return Err(format!(
                "module {module} binds support chunk {key} over a hidden table"
            ));
        }
        if self.support_chunks.insert(key.to_owned()) {
            Ok(())
        } else {
            Err(format!(
                "module {module} registers duplicate support chunk {key}"
            ))
        }
    }

    fn insert_host_type(&mut self, module: &str, name: &str) -> Result<(), String> {
        if self.host_types.insert(name.to_owned()) {
            Ok(())
        } else {
            Err(format!(
                "module {module} registers duplicate host type {name}"
            ))
        }
    }

    /// Whether the declared shape matches the runtime-registered shape. Hidden
    /// bindings and the override flag are runtime-binding metadata with no
    /// declaration syntax, so only globals, libraries, and native require exports
    /// are compared; an overridden global must still be declared (it is in
    /// `globals`), and a hidden table must not be (a declared-but-unregistered
    /// global fails the match).
    fn matches_bindings(&self, other: &Self) -> bool {
        self.globals == other.globals
            && self.libraries == other.libraries
            && self.module_exports == other.module_exports
    }

    fn collect_module_value_shape(
        &mut self,
        walk: ShapeWalk<'_>,
        prefix: &str,
        value: &ModuleValue,
    ) -> Result<(), String> {
        let ModuleValue::Table(table) = value else {
            return Ok(());
        };
        for entry in &table.entries {
            let path = walk.member_path(prefix, entry.name.as_ref());
            let kind = module_value_kind(&entry.value);
            self.insert_library_member(walk.module, walk.root, &path, kind)?;
            self.collect_module_value_shape(walk, &path, &entry.value)?;
        }
        Ok(())
    }

    fn collect_module_export_value_shape(
        &mut self,
        walk: ShapeWalk<'_>,
        prefix: &str,
        value: &ModuleValue,
    ) -> Result<(), String> {
        let ModuleValue::Table(table) = value else {
            return Ok(());
        };
        for entry in &table.entries {
            let path = walk.member_path(prefix, entry.name.as_ref());
            let kind = module_value_kind(&entry.value);
            self.insert_module_export_member(walk.module, &path, kind)?;
            self.collect_module_export_value_shape(walk, &path, &entry.value)?;
        }
        Ok(())
    }

    fn collect_declared_table_shape(
        &mut self,
        walk: ShapeWalk<'_>,
        prefix: &str,
        props: &[TableProp],
    ) -> Result<(), String> {
        for prop in props {
            let path = walk.member_path(prefix, prop.name.as_str());
            let kind = type_binding_kind(&prop.prop_type);
            self.insert_library_member(walk.module, walk.root, &path, kind)?;
            if let Some(props) = table_props(&prop.prop_type) {
                self.collect_declared_table_shape(walk, &path, props)?;
            }
        }
        Ok(())
    }

    fn merge_from(&mut self, module: &str, shape: &Self) -> Result<(), String> {
        for (global, kind) in &shape.globals {
            if self.globals.get(global) == Some(kind)
                && *kind == HostBindingKind::Table
                && self.library_roots.contains(global)
                && shape.library_roots.contains(global)
            {
                continue;
            }
            self.insert_global(module, global, *kind)?;
            if shape.library_roots.contains(global) {
                self.library_roots.insert(global.clone());
            }
        }
        for (library, members) in &shape.libraries {
            for (member, kind) in members {
                self.insert_library_member(module, library, member, *kind)?;
            }
        }
        for (export, members) in &shape.module_exports {
            if self.module_exports.contains_key(export) {
                return Err(format!("duplicate native require export {export}"));
            }
            for (member, kind) in members {
                self.insert_module_export_member(module, member, *kind)?;
            }
        }
        for (table, members) in &shape.hidden {
            if self.support_chunks.contains(table) {
                return Err(format!(
                    "hidden table {table} collides with a support chunk"
                ));
            }
            for (member, kind) in members {
                self.insert_hidden_member(module, table, member, *kind)?;
            }
        }
        for key in &shape.support_chunks {
            self.insert_support_chunk(module, key)?;
        }
        for host_type in &shape.host_types {
            self.insert_host_type(module, host_type)?;
        }
        Ok(())
    }
}

struct HostModuleAuditBuilder {
    module: String,
    export: ModuleExport,
    shape: HostModuleShape,
    errors: Vec<String>,
}

impl HostModuleAuditBuilder {
    fn new(module: &str, export: ModuleExport) -> Self {
        Self {
            module: module.to_owned(),
            export,
            shape: HostModuleShape::default(),
            errors: Vec::new(),
        }
    }

    fn finish(self) -> Result<HostModuleShape, String> {
        if self.errors.is_empty() {
            Ok(self.shape)
        } else {
            Err(self.errors.join("; "))
        }
    }

    fn record_function(&mut self, name: &str, binding: &ModuleBinding) {
        if self.record_module_export(name, binding, HostBindingKind::Function) {
            return;
        }
        let result = match binding {
            ModuleBinding::Global => {
                self.shape
                    .insert_global(&self.module, name, HostBindingKind::Function)
            }
            ModuleBinding::GlobalOverride => self
                .shape
                .insert_global(&self.module, name, HostBindingKind::Function)
                .map(|()| {
                    self.shape.overrides.insert(name.to_owned());
                }),
            ModuleBinding::Library(library) => self
                .shape
                .ensure_library_root(&self.module, library)
                .and_then(|()| {
                    self.shape.insert_library_member(
                        &self.module,
                        library,
                        name,
                        HostBindingKind::Function,
                    )
                }),
            ModuleBinding::Hidden(table) => self.shape.insert_hidden_member(
                &self.module,
                table,
                name,
                HostBindingKind::Function,
            ),
        };
        if let Err(error) = result {
            self.errors.push(error);
        }
    }

    fn record_module_export(
        &mut self,
        name: &str,
        binding: &ModuleBinding,
        kind: HostBindingKind,
    ) -> bool {
        if matches!(binding, ModuleBinding::Hidden(_))
            || matches!(self.export, ModuleExport::Globals)
        {
            return false;
        }
        if let Err(error) = self
            .shape
            .insert_module_export_member(&self.module, name, kind)
        {
            self.errors.push(error);
        }
        self.export == ModuleExport::Require
    }
}

impl ModuleBuilder for HostModuleAuditBuilder {
    fn function(&mut self, name: &str, binding: ModuleBinding, _f: Box<dyn HostFunction>) {
        self.record_function(name, &binding);
    }

    fn host_callable(
        &mut self,
        name: &str,
        binding: ModuleBinding,
        _f: Box<dyn std::any::Any + Send + Sync>,
    ) {
        self.record_function(name, &binding);
    }

    fn constant(&mut self, name: &str, binding: ModuleBinding, value: ModuleValue) {
        let kind = module_value_kind(&value);
        if !matches!(binding, ModuleBinding::Hidden(_))
            && !matches!(self.export, ModuleExport::Globals)
            && let Err(error) = self.shape.collect_module_export_value_shape(
                ShapeWalk {
                    module: &self.module,
                    root: &self.module,
                },
                name,
                &value,
            )
        {
            self.errors.push(error);
            return;
        }
        if self.record_module_export(name, &binding, kind) {
            return;
        }
        let result = match &binding {
            ModuleBinding::Global | ModuleBinding::GlobalOverride => {
                let overrides = matches!(binding, ModuleBinding::GlobalOverride);
                self.shape
                    .insert_global(&self.module, name, kind)
                    .and_then(|()| {
                        if overrides {
                            self.shape.overrides.insert(name.to_owned());
                        }
                        self.shape.collect_module_value_shape(
                            ShapeWalk {
                                module: &self.module,
                                root: name,
                            },
                            "",
                            &value,
                        )
                    })
            }
            ModuleBinding::Library(library) => self
                .shape
                .ensure_library_root(&self.module, library)
                .and_then(|()| {
                    self.shape
                        .insert_library_member(&self.module, library, name, kind)
                })
                .and_then(|()| {
                    self.shape.collect_module_value_shape(
                        ShapeWalk {
                            module: &self.module,
                            root: library.as_ref(),
                        },
                        name,
                        &value,
                    )
                }),
            // Hidden constants are host-facing only: record the member for
            // duplicate detection, but walk no nested shape — hidden bindings
            // carry no declaration obligation.
            ModuleBinding::Hidden(table) => {
                self.shape
                    .insert_hidden_member(&self.module, table, name, kind)
            }
        };
        if let Err(error) = result {
            self.errors.push(error);
        }
    }

    fn host_type(&mut self, ty: Box<dyn std::any::Any + Send + Sync>) {
        let Ok(ty) = ty.downcast::<HostType>() else {
            self.errors
                .push("host_type payload was not an ruau-vm HostType".to_owned());
            return;
        };
        if let Err(error) = self.shape.insert_host_type(&self.module, ty.name()) {
            self.errors.push(error);
        }
    }

    fn support_chunk(&mut self, registry_key: &str, _source: &[u8]) {
        if let Err(error) = self.shape.insert_support_chunk(&self.module, registry_key) {
            self.errors.push(error);
        }
    }
}

fn module_value_kind(value: &ModuleValue) -> HostBindingKind {
    match value {
        ModuleValue::Table(_) => HostBindingKind::Table,
        ModuleValue::Nil
        | ModuleValue::Boolean(_)
        | ModuleValue::Number(_)
        | ModuleValue::Integer(_)
        | ModuleValue::LightUserdata { .. }
        | ModuleValue::Bytes(_) => HostBindingKind::Value,
    }
}

/// The fixed coordinates of one shape-collection walk: the host module
/// being audited and the library root the walk descends from. The growing
/// member path travels as its own parameter so the three same-typed strings
/// cannot be swapped at a recursive call.
#[derive(Clone, Copy)]
struct ShapeWalk<'a> {
    module: &'a str,
    root: &'a str,
}

impl ShapeWalk<'_> {
    fn member_path(self, prefix: &str, name: &str) -> String {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}.{name}")
        }
    }
}

fn validate_host_modules(
    capabilities: &RuntimeCapabilities,
    modules: &[Arc<dyn NativeModule>],
) -> Result<Vec<DefinitionModule>, ConfigError> {
    let mut declarations = Vec::with_capacity(modules.len());
    let mut shapes = Vec::with_capacity(modules.len());
    let mut all_bindings = HostModuleShape::default();
    let builtin_globals = runtime_capability_builtin_global_names(capabilities);
    for module in modules {
        let declaration = module.declaration().render();
        let declared =
            declared_host_module_shape(module.name(), &declaration).map_err(|reason| {
                ConfigError::InvalidHostModuleDeclaration {
                    module: module.name().to_owned(),
                    reason,
                }
            })?;
        let mut builder = HostModuleAuditBuilder::new(module.name(), module.export());
        module.build(&mut builder);
        let runtime =
            builder
                .finish()
                .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                    module: module.name().to_owned(),
                    reason,
                })?;
        let expected = declared_runtime_shape(module.name(), module.export(), &declared).map_err(
            |reason| ConfigError::InvalidHostModuleDeclaration {
                module: module.name().to_owned(),
                reason,
            },
        )?;
        if !expected.matches_bindings(&runtime) {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.name().to_owned(),
                reason: host_module_shape_mismatch(&expected, &runtime),
            });
        }
        reject_surface_omitted_host_bindings(capabilities, module.name(), &runtime)?;
        reject_unflagged_builtin_collisions(&builtin_globals, module.name(), &runtime)?;
        all_bindings
            .merge_from(module.name(), &runtime)
            .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                module: module.name().to_owned(),
                reason,
            })?;
        declarations.push(DefinitionModule {
            name: module.name().to_owned().into(),
            source: declaration.into_owned().into(),
        });
        shapes.push((module.name().to_owned(), expected));
    }
    validate_host_module_declaration_types(capabilities, &declarations, &shapes)?;
    Ok(declarations)
}

fn declared_runtime_shape(
    module: &str,
    export: ModuleExport,
    declared: &HostModuleShape,
) -> Result<HostModuleShape, String> {
    let mut shape = HostModuleShape {
        globals: declared.globals.clone(),
        libraries: declared.libraries.clone(),
        library_roots: declared.library_roots.clone(),
        module_exports: BTreeMap::new(),
        overrides: BTreeSet::new(),
        hidden: BTreeMap::new(),
        host_types: BTreeSet::new(),
        support_chunks: BTreeSet::new(),
    };
    if export == ModuleExport::Globals {
        return Ok(shape);
    }
    let Some(kind) = shape.globals.get(module).copied() else {
        return Err(format!(
            "module export mode {export:?} requires declaration table `{module}`"
        ));
    };
    if kind != HostBindingKind::Table {
        return Err(format!(
            "module export mode {export:?} requires `{module}` to be declared as a table"
        ));
    }
    let members = shape.libraries.get(module).cloned().unwrap_or_default();
    shape.module_exports.insert(module.to_owned(), members);
    if export == ModuleExport::Require {
        shape.globals.remove(module);
        shape.libraries.remove(module);
        shape.library_roots.remove(module);
    }
    Ok(shape)
}

/// The builtin global names the checker environment defines for these runtime
/// capabilities before any host-module declaration is merged.
fn runtime_capability_builtin_global_names(capabilities: &RuntimeCapabilities) -> BTreeSet<String> {
    let mut arena = Arena::new();
    builtin_environment_for(capabilities, &mut arena)
        .globals()
        .map(|global| global.name.clone())
        .collect()
}

/// Global bindings are fail-closed about the surface's builtin set: a
/// plain `Global` colliding with a builtin requires the explicit
/// `GlobalOverride` opt-in, and an override must have a builtin to replace.
fn reject_unflagged_builtin_collisions(
    builtin_globals: &BTreeSet<String>,
    module: &str,
    shape: &HostModuleShape,
) -> Result<(), ConfigError> {
    for global in shape.globals.keys() {
        // A library root shared with a surface library (a module extending
        // `string`, say) is the documented library-extension path, not a
        // global replacement.
        if shape.library_roots.contains(global) {
            continue;
        }
        let collides = builtin_globals.contains(global);
        let overrides = shape.overrides.contains(global);
        if collides && !overrides {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!(
                    "global {global} collides with a surface builtin; replacing it \
                     requires the explicit ModuleBinding::GlobalOverride opt-in"
                ),
            });
        }
        if overrides && !collides {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!(
                    "global {global} is bound as an override, but the surface \
                     installs no builtin of that name to replace"
                ),
            });
        }
    }
    Ok(())
}

fn reject_surface_omitted_host_bindings(
    capabilities: &RuntimeCapabilities,
    module: &str,
    shape: &HostModuleShape,
) -> Result<(), ConfigError> {
    for library in capabilities.omitted_libraries() {
        let name = library.global_name();
        if shape.globals.contains_key(name) || shape.libraries.contains_key(name) {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!("binds omitted surface library {name}"),
            });
        }
    }
    Ok(())
}

fn host_module_manifest_version(
    modules: &[Arc<dyn NativeModule>],
    declarations: &[DefinitionModule],
) -> u64 {
    let mut hash = FNV1A64_OFFSET;
    for (index, declaration) in declarations.iter().enumerate() {
        fnv1a64_update(&mut hash, declaration.name.as_bytes());
        fnv1a64_update(&mut hash, b"\0");
        let export = modules
            .get(index)
            .map_or("DeclarationOnly", |module| match module.export() {
                ModuleExport::Globals => "Globals",
                ModuleExport::Require => "Require",
                ModuleExport::Both => "Both",
            });
        fnv1a64_update(&mut hash, export.as_bytes());
        fnv1a64_update(&mut hash, b"\0");
        fnv1a64_update(&mut hash, declaration.source.as_bytes());
        fnv1a64_update(&mut hash, b"\0");
    }
    hash
}

const FNV1A64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a64_update(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV1A64_PRIME);
    }
}

pub(crate) fn validate_host_module_declaration_types(
    capabilities: &RuntimeCapabilities,
    declarations: &[DefinitionModule],
    shapes: &[(String, HostModuleShape)],
) -> Result<(), ConfigError> {
    let mut arena = Arena::new();
    let builtins =
        builtin_environment_for_with_definition_modules(capabilities, &mut arena, declarations);
    for (module, shape) in shapes {
        for (global, kind) in &shape.globals {
            let Some(global_ty) = builtins.global(global).map(|global| global.ty) else {
                return Err(ConfigError::InvalidHostModuleDeclaration {
                    module: module.clone(),
                    reason: format!(
                        "declaration did not install global {global}; check its type annotations"
                    ),
                });
            };
            validate_declared_binding_type(
                module,
                &arena,
                global_ty,
                *kind,
                &format!("global {global}"),
            )?;
        }
        for (library, members) in &shape.libraries {
            let Some(library_ty) = builtins.global(library).map(|global| global.ty) else {
                return Err(ConfigError::InvalidHostModuleDeclaration {
                    module: module.clone(),
                    reason: format!(
                        "declaration did not install library {library}; check its type annotations"
                    ),
                });
            };
            for (member, kind) in members {
                let Some(member_ty) = table_property_path_type(&arena, library_ty, member) else {
                    return Err(ConfigError::InvalidHostModuleDeclaration {
                        module: module.clone(),
                        reason: format!(
                            "declaration did not install library binding {library}.{member}; check its type annotations"
                        ),
                    });
                };
                validate_declared_binding_type(
                    module,
                    &arena,
                    member_ty,
                    *kind,
                    &format!("library binding {library}.{member}"),
                )?;
            }
        }
        for (export, members) in &shape.module_exports {
            let Some(export_ty) = builtins.global(export).map(|global| global.ty) else {
                return Err(ConfigError::InvalidHostModuleDeclaration {
                    module: module.clone(),
                    reason: format!(
                        "declaration did not install module export {export}; check its type annotations"
                    ),
                });
            };
            validate_declared_binding_type(
                module,
                &arena,
                export_ty,
                HostBindingKind::Table,
                &format!("module export {export}"),
            )?;
            for (member, kind) in members {
                let Some(member_ty) = table_property_path_type(&arena, export_ty, member) else {
                    return Err(ConfigError::InvalidHostModuleDeclaration {
                        module: module.clone(),
                        reason: format!(
                            "declaration did not install module export binding {export}.{member}; check its type annotations"
                        ),
                    });
                };
                validate_declared_binding_type(
                    module,
                    &arena,
                    member_ty,
                    *kind,
                    &format!("module export binding {export}.{member}"),
                )?;
            }
        }
    }
    Ok(())
}

fn validate_declaration_globals(
    capabilities: &RuntimeCapabilities,
    module_declarations: &[DefinitionModule],
    globals: &[DeclarationGlobalSpec],
) -> Result<(), ConfigError> {
    let mut seen = BTreeSet::new();
    let mut declarations = module_declarations.to_vec();
    for global in globals {
        if !seen.insert(global.name.clone()) {
            return Err(ConfigError::InvalidDeclarationGlobal {
                name: global.name.clone(),
                reason: "duplicate declaration-only global".to_owned(),
            });
        }
        let shape =
            declared_host_module_shape(&global.name, &global.source()).map_err(|reason| {
                ConfigError::InvalidDeclarationGlobal {
                    name: global.name.clone(),
                    reason,
                }
            })?;
        if !shape.globals.contains_key(&global.name) {
            return Err(ConfigError::InvalidDeclarationGlobal {
                name: global.name.clone(),
                reason: "generated declaration did not define the requested global".to_owned(),
            });
        }
        declarations.push(global.definition_module());
    }

    let mut arena = Arena::new();
    let builtins =
        builtin_environment_for_with_definition_modules(capabilities, &mut arena, &declarations);
    let mut checker = Checker::with_builtins(arena, builtins);
    for global in globals {
        checker
            .require_global(&global.name, &global.type_text)
            .map_err(|diagnostics| ConfigError::InvalidDeclarationGlobal {
                name: global.name.clone(),
                reason: diagnostics.render("<declaration-global>"),
            })?;
    }
    Ok(())
}

fn validate_declared_binding_type(
    module: &str,
    arena: &Arena,
    ty: TypeId,
    kind: HostBindingKind,
    label: &str,
) -> Result<(), ConfigError> {
    let valid = match kind {
        HostBindingKind::Function => is_callable_type(arena, ty),
        HostBindingKind::Value => true,
        HostBindingKind::Table => is_table_type(arena, ty),
    };
    if valid {
        return Ok(());
    }
    let expected = match kind {
        HostBindingKind::Function => "function",
        HostBindingKind::Value => "value",
        HostBindingKind::Table => "table",
    };
    Err(ConfigError::InvalidHostModuleDeclaration {
        module: module.to_owned(),
        reason: format!("declaration for {label} is not a {expected} type"),
    })
}

fn is_callable_type(arena: &Arena, ty: TypeId) -> bool {
    TypeView::new(arena, ty).is_callable()
}

fn is_table_type(arena: &Arena, ty: TypeId) -> bool {
    TypeView::new(arena, ty).is_table_like()
}

fn table_property_path_type(arena: &Arena, ty: TypeId, path: &str) -> Option<TypeId> {
    TypeView::new(arena, ty)
        .property_path(path)
        .map(|view| view.id())
}

fn declared_host_module_shape(module: &str, source: &str) -> Result<HostModuleShape, String> {
    let parsed = parse_file_with(
        source,
        Options {
            allow_declaration_syntax: true,
            capture_comments: true,
            ..Options::default()
        },
        SyntaxFlags::all_luau(),
    );
    if !parsed.errors.is_empty() {
        let errors = parsed
            .errors
            .iter()
            .map(|error| format!("{error:?}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("declaration parse failed: {errors}"));
    }
    let Some(root) = parsed.root else {
        return Err("declaration did not parse a root block".to_owned());
    };
    let mut shape = HostModuleShape::default();
    collect_declared_host_bindings(module, &root, &mut shape)?;
    Ok(shape)
}

fn collect_declared_host_bindings(
    module: &str,
    stat: &Stat,
    shape: &mut HostModuleShape,
) -> Result<(), String> {
    match stat {
        Stat::Block { body, .. } => {
            for stat in body {
                collect_declared_host_bindings(module, stat, shape)?;
            }
        }
        Stat::DeclareFunction { name, .. } => {
            shape.insert_global(module, name.as_str(), HostBindingKind::Function)?;
        }
        Stat::DeclareGlobal {
            name, luau_type, ..
        } => {
            let kind = type_binding_kind(luau_type);
            shape.insert_global(module, name.as_str(), kind)?;
            if let Some(props) = table_props(luau_type) {
                shape.collect_declared_table_shape(
                    ShapeWalk {
                        module,
                        root: name.as_str(),
                    },
                    "",
                    props,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn type_binding_kind(ty: &Type) -> HostBindingKind {
    match ty {
        Type::Function { .. } => HostBindingKind::Function,
        Type::Table { .. } => HostBindingKind::Table,
        Type::Group { inner, .. } => type_binding_kind(inner),
        _ => HostBindingKind::Value,
    }
}

fn table_props(ty: &Type) -> Option<&[TableProp]> {
    match ty {
        Type::Table { props, .. } => Some(props.as_slice()),
        Type::Group { inner, .. } => table_props(inner),
        _ => None,
    }
}

fn host_module_shape_mismatch(declared: &HostModuleShape, runtime: &HostModuleShape) -> String {
    let mut parts = Vec::new();
    add_binding_delta(
        &mut parts,
        "declares globals not registered at runtime",
        &declared.globals,
        &runtime.globals,
    );
    add_binding_delta(
        &mut parts,
        "registers globals missing from declaration",
        &runtime.globals,
        &declared.globals,
    );
    for (library, declared_members) in &declared.libraries {
        let Some(runtime_members) = runtime.libraries.get(library) else {
            parts.push(format!(
                "declares library {library} but registers no runtime bindings for it"
            ));
            continue;
        };
        add_binding_delta(
            &mut parts,
            &format!("declares {library} members not registered at runtime"),
            declared_members,
            runtime_members,
        );
        add_binding_delta(
            &mut parts,
            &format!("registers {library} members missing from declaration"),
            runtime_members,
            declared_members,
        );
    }
    for library in runtime.libraries.keys() {
        if !declared.libraries.contains_key(library) {
            parts.push(format!(
                "registers library {library} but declaration has no table for it"
            ));
        }
    }
    for (export, declared_members) in &declared.module_exports {
        let Some(runtime_members) = runtime.module_exports.get(export) else {
            parts.push(format!(
                "declares module export {export} but registers no runtime bindings for it"
            ));
            continue;
        };
        add_binding_delta(
            &mut parts,
            &format!("declares {export} exports not registered at runtime"),
            declared_members,
            runtime_members,
        );
        add_binding_delta(
            &mut parts,
            &format!("registers {export} exports missing from declaration"),
            runtime_members,
            declared_members,
        );
    }
    for export in runtime.module_exports.keys() {
        if !declared.module_exports.contains_key(export) {
            parts.push(format!(
                "registers module export {export} but declaration has no table for it"
            ));
        }
    }
    parts.join("; ")
}

fn add_binding_delta(
    parts: &mut Vec<String>,
    label: &str,
    left: &BTreeMap<String, HostBindingKind>,
    right: &BTreeMap<String, HostBindingKind>,
) {
    let missing = left
        .keys()
        .filter(|name| !right.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        parts.push(format!("{label}: {}", missing.join(", ")));
    }
    let mismatched = left
        .iter()
        .filter_map(|(name, kind)| {
            let other = right.get(name)?;
            (kind != other).then(|| {
                format!(
                    "{name} ({} vs {})",
                    binding_kind_label(*kind),
                    binding_kind_label(*other)
                )
            })
        })
        .collect::<Vec<_>>();
    if !mismatched.is_empty() {
        parts.push(format!(
            "{label} with different kind: {}",
            mismatched.join(", ")
        ));
    }
}

fn binding_kind_label(kind: HostBindingKind) -> &'static str {
    match kind {
        HostBindingKind::Function => "function",
        HostBindingKind::Value => "value",
        HostBindingKind::Table => "table",
    }
}
