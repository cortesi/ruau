//! Runtime and checker surface configuration.
//!
//! A [`Surface`] combines runtime capabilities, native modules, declaration
//! modules, and optional `require` source.

use std::{collections::BTreeMap, error::Error, fmt, sync::Arc};

use ruau_analysis::{
    ParseGraphResult,
    resolve::{AnalysisMode, config::EmptyResolver},
};
use ruau_bytecode::{BytecodeChunk, CompileError, CompileOptions};
use ruau_decl as decl;
use ruau_source::{ModuleName, ModuleSource, RootOverlaySource, Source};
use ruau_typecheck::{
    builtins::{BuiltinEnvironment, DefinitionModule},
    checker::{CheckedModule, Checker, Config, ConformanceCheck},
    diagnostics::GraphDiagnostics,
    frontend::GraphChecker,
    types::{Arena, TypeId},
};
use ruau_vm::{Ambient, Library, Limits, RuntimeCapabilities, VmBuilder, VmSandboxPolicy};
use ruau_vm_api::{ModuleExport, NativeModule};

mod audit;
mod prepare;

pub use prepare::{
    PrepareDiagnosticPolicy, PrepareError, PrepareOptions, PreparedRunError, PreparedScript,
};

static EMPTY_CONFIG_RESOLVER: EmptyResolver = EmptyResolver;

pub(crate) fn builtin_environment_for(
    capabilities: &RuntimeCapabilities,
    arena: &mut Arena,
) -> BuiltinEnvironment {
    builtin_environment_for_with_definition_modules(capabilities, arena, &[])
}

pub(crate) fn builtin_environment_for_with_definition_modules(
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

/// Surface configuration error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
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
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ruau configuration error: ")?;
        match self {
            Self::InvalidHostModuleDeclaration { module, reason } => {
                write!(f, "host module {module} declaration is invalid: {reason}")
            }
            Self::InvalidRequiredGlobal { name, reason } => {
                write!(f, "required global {name} is invalid: {reason}")
            }
            Self::InvalidDeclarationGlobal { name, reason } => {
                write!(f, "declaration global {name} is invalid: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Surface graph-checking error: the surface has no module source for an
/// existing-module graph check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphCheckError;

impl fmt::Display for GraphCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("surface graph check requires a module source or a synthetic root source")
    }
}

impl Error for GraphCheckError {}

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
pub(crate) struct DeclarationGlobalSpec {
    pub(crate) name: String,
    pub(crate) type_text: String,
}

#[derive(Clone)]
struct SurfaceCheckerBase {
    arena: Arena,
    builtins: BuiltinEnvironment,
    ambient_require_returns: Vec<(String, TypeId)>,
}

impl DeclarationGlobalSpec {
    pub(crate) fn source(&self) -> String {
        format!("declare {}: {}", self.name, self.type_text)
    }

    pub(crate) fn definition_module(&self) -> DefinitionModule {
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
            audit::host_module_manifest_version(&modules, &module_declarations);
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
    pub fn check_str(&self, source: &str) -> CheckedModule {
        self.check_str_with_config(source, Config::default())
    }

    /// Checks source text using this surface's builtin environment and an
    /// explicit checker configuration.
    ///
    /// If `config` does not already force a source mode, this method fills the
    /// override from [`Self::analysis_mode`]. Caller-provided overrides win.
    #[must_use]
    pub fn check_str_with_config(&self, source: &str, config: Config) -> CheckedModule {
        let mut checker = self.new_checker();
        checker.check_source_with_config(source, self.surface_config(config))
    }

    /// Checks arbitrary source bytes using this surface's environment and
    /// analysis mode.
    #[must_use]
    pub fn check_bytes(&self, source: &[u8]) -> CheckedModule {
        self.check_bytes_with_config(source, Config::default())
    }

    /// Checks arbitrary source bytes using this surface's builtin environment
    /// and an explicit checker configuration.
    ///
    /// If `config` does not already force a source mode, this method fills the
    /// override from [`Self::analysis_mode`]. Caller-provided overrides win.
    #[must_use]
    fn check_bytes_with_config(&self, source: &[u8], config: Config) -> CheckedModule {
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
        if let Some(text) = source.as_str() {
            self.check_str_with_config(text, config)
        } else {
            self.check_bytes_with_config(source.as_bytes(), config)
        }
    }

    /// Checks an existing module-source root and its statically reachable graph.
    ///
    /// This ready-only bridge reports pending async source futures as resolver
    /// diagnostics in the returned graph. Use [`Self::check_module_graph_async`]
    /// to await async source reads and resolutions.
    ///
    /// # Errors
    /// Returns [`GraphCheckError`] when this surface has no module source
    /// installed.
    pub fn check_module_graph(
        &self,
        root: impl Into<ModuleName>,
    ) -> Result<CheckedGraph, GraphCheckError> {
        let Some(source) = self.module_source() else {
            return Err(GraphCheckError);
        };
        let mut frontend = self.graph_checker(source.as_ref());
        let result = frontend.check(root);
        Ok(CheckedGraph::from_frontend(&frontend, result))
    }

    /// Checks an existing module-source root and awaits async source futures.
    ///
    /// # Errors
    /// Returns [`GraphCheckError`] when this surface has no module source
    /// installed.
    pub async fn check_module_graph_async(
        &self,
        root: impl Into<ModuleName>,
    ) -> Result<CheckedGraph, GraphCheckError> {
        let Some(source) = self.module_source() else {
            return Err(GraphCheckError);
        };
        let mut frontend = self.graph_checker(source.as_ref());
        let result = frontend.check_async(root).await;
        Ok(CheckedGraph::from_frontend(&frontend, result))
    }

    /// Checks a synthetic root source plus dependencies from this surface's
    /// optional module source.
    ///
    /// This ready-only bridge reports pending async source futures as resolver
    /// diagnostics in the returned graph. Use [`Self::check_graph_async`]
    /// to await async source reads and resolutions.
    #[must_use]
    pub fn check_graph(&self, source: &Source) -> CheckedGraph {
        let source = self.root_overlay_source(source);
        let root = source.root_name();
        let mut frontend = self.graph_checker(&source);
        let result = frontend.check(root);
        CheckedGraph::from_frontend(&frontend, result)
    }

    /// Checks a synthetic root source plus dependencies from this surface's
    /// optional module source, awaiting async source futures.
    pub async fn check_graph_async(&self, source: &Source) -> CheckedGraph {
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
        let mut overlay = RootOverlaySource::new(source.id().clone(), source.as_bytes().to_vec())
            .with_display_name(source.display_name().to_owned())
            .with_root_requester(source.id().clone())
            .reject_delegate_root_id_collision(true);
        if let Some(module_source) = self.module_source() {
            overlay = overlay.with_owned_delegate(module_source);
        }
        overlay
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

    /// Compiles raw source bytes under this surface's runtime capabilities.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_bytes(&self, source: &[u8]) -> Result<BytecodeChunk, CompileError> {
        self.compile_bytes_with_options(source, &CompileOptions::default())
    }

    /// Compiles raw source bytes under this surface's runtime capabilities
    /// with an explicit public compile policy.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_bytes_with_options(
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
    pub fn compile(&self, source: &Source) -> Result<BytecodeChunk, CompileError> {
        self.compile_with_options(source, &CompileOptions::default())
    }

    /// Compiles a named source under this surface's runtime capabilities with
    /// an explicit public compile policy.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_with_options(
        &self,
        source: &Source,
        base: &CompileOptions,
    ) -> Result<BytecodeChunk, CompileError> {
        self.compile_bytes_with_options(source.as_bytes(), base)
    }

    /// Compiles and validates raw source bytes into a
    /// [`CompiledModule`](ruau_vm::CompiledModule).
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_module_bytes(
        &self,
        source: &[u8],
    ) -> Result<ruau_vm::CompiledModule, CompileError> {
        self.runtime_capabilities()
            .compile_module(source, &CompileOptions::default())
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
        let module_declarations =
            audit::validate_host_modules(&runtime_capabilities, &self.modules)?;
        audit::validate_declaration_globals(
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
