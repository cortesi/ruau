//! Runtime and checker surface configuration.
//!
//! A [`Surface`] is the shared description of what host Luau code may see. It
//! combines runtime capabilities, native modules, declaration modules,
//! declaration-only globals, required global exports, compile options, and an
//! optional `require` source.
//!
//! The common flow is check -> prepare -> run: validate source against the
//! surface, compile it under matching [`ruau_vm::RuntimeCapabilities`], then
//! run the resulting [`PreparedSource`] in a VM. Graph checks use the same
//! checker configuration over a [`ruau_source::SourceProvider`] and return a
//! [`CheckedGraph`] with module-qualified diagnostics. Hosts that manage their
//! own execution can also ask the surface for a configured VM builder or a
//! borrow-based [`ruau_vm::CompiledModule`].

use std::{
    collections::BTreeSet,
    error::Error,
    fmt,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use ruau_bytecode::{BytecodeChunk, CompileError, CompileOptions};
use ruau_declaration as decl;
use ruau_source::{ModuleName, RootSource, SnapshotSource, Source, SourceProvider};
use ruau_syntax::{
    Expr, Stat,
    parse::{Config as ParseConfig, ParsedModule, parse_with_config},
    transform::{EraseDeclarationsOptions, erase_declarations},
};
use ruau_typecheck::{
    CheckedGraph, CheckedModule, Checker, Config, ConformanceCheck, GraphChecker, GraphLimitError,
    GraphLimits, Mode,
    builtins::{DefinitionModule, Environment, TypeScope},
    config::EmptyResolver,
    types::{Arena, TypeId},
};
use ruau_vm::{
    Ambient, Library, Limits, ModuleExport, NativeModule, RuntimeCapabilities,
    SourceModuleExportPolicy, VmBuilder, VmSandboxPolicy,
};

mod audit;
mod prepare;
mod typed_source;

pub use prepare::{
    PrepareDiagnosticPolicy, PrepareError, PrepareGraphError, PrepareOptions, PreparedContextError,
    PreparedGraph, PreparedGraphRunError, PreparedLoadError, PreparedRunError, PreparedSource,
};
use typed_source::{TypedModuleEntry, TypedModuleSource};

static EMPTY_CONFIG_RESOLVER: EmptyResolver = EmptyResolver;

/// Returns whether a compilable module source has a top-level `return`.
fn source_has_top_level_return(source: &str) -> bool {
    let parsed =
        ruau_syntax::parse::parse_with_config(source, &ruau_syntax::parse::Config::default());
    if !parsed.errors.is_empty() {
        return false;
    }
    match parsed.root {
        ruau_syntax::Stat::Block { body, .. } => body
            .iter()
            .any(|stat| matches!(stat, ruau_syntax::Stat::Return { .. })),
        ruau_syntax::Stat::Return { .. } => true,
        _ => false,
    }
}

/// Erase a public declaration while retaining the declared type of its returned native global.
fn typed_module_require_source(source: &str) -> Result<String, String> {
    let config = ParseConfig {
        allow_declaration_syntax: true,
        ..ParseConfig::default()
    };
    let parsed = parse_with_config(source, &config);
    if !parsed.errors.is_empty() {
        return Err(parsed
            .errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "));
    }
    let Stat::Block { body, .. } = &parsed.root else {
        return Err("public declaration must be a top-level statement block".to_owned());
    };
    let returned = body.iter().rev().find_map(|stat| match stat {
        Stat::Return {
            location: Some(location),
            list,
        } if list.len() == 1 => match &list[0] {
            Expr::Global { name, .. } => Some((name.as_str(), *location)),
            _ => None,
        },
        _ => None,
    });
    let declaration = returned.and_then(|(returned_name, return_location)| {
        body.iter()
            .find_map(|stat| match stat {
                Stat::DeclareGlobal {
                    name,
                    declared_type,
                    ..
                } if name.as_str() == returned_name => {
                    Some((name.as_str(), declared_type.location()?))
                }
                _ => None,
            })
            .map(|declaration| (declaration, (returned_name, return_location)))
    });
    let mut erased =
        erase_declarations(source, EraseDeclarationsOptions::default()).map_err(|errors| {
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        })?;
    if let Some(((_, type_location), (returned_name, return_location))) = declaration
        && let (Some(type_range), Some(return_range)) = (
            type_location.byte_range(source),
            return_location.byte_range(source),
        )
    {
        let declared_type = &source[type_range];
        erased.replace_range(
            return_range,
            &format!("return {returned_name} :: ({declared_type})"),
        );
        return Ok(erased);
    }
    Err("public declaration must return its declared global".to_owned())
}

pub(crate) fn builtin_environment_for(
    capabilities: &RuntimeCapabilities,
    arena: &mut Arena,
) -> Environment {
    builtin_environment_for_with_definition_modules(capabilities, arena, &[])
        .expect("builtin environment builds without definition modules")
}

pub(crate) fn builtin_environment_for_with_definition_modules(
    capabilities: &RuntimeCapabilities,
    arena: &mut Arena,
    definition_modules: &[DefinitionModule],
) -> Result<Environment, ConfigError> {
    let omitted_libraries = capabilities.omitted_libraries().map(Library::global_name);
    let omitted_runtime_compilation =
        (!capabilities.runtime_compilation_enabled()).then_some("loadstring");
    let environment = Environment::standard_with_definition_modules(arena, definition_modules)
        .map_err(|error| ConfigError::InvalidDeclarationModule {
            module: error.module,
            reason: format!(
                "declaration did not parse: {}",
                error
                    .errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        })?;
    Ok(environment.without_globals(omitted_libraries.chain(omitted_runtime_compilation)))
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
    /// A required root return type failed to parse or resolve.
    InvalidRequiredReturn {
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
    /// A declaration-only checker module failed to parse or validate.
    InvalidDeclarationModule {
        /// Stable declaration module name.
        module: String,
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
            Self::InvalidRequiredReturn { reason } => {
                write!(f, "required root return is invalid: {reason}")
            }
            Self::InvalidDeclarationGlobal { name, reason } => {
                write!(f, "declaration global {name} is invalid: {reason}")
            }
            Self::InvalidDeclarationModule { module, reason } => {
                write!(f, "declaration module {module} is invalid: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Hard failure raised while starting or bounding a surface graph check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphCheckError {
    /// An existing-module graph check had no installed source provider.
    MissingModuleSource,
    /// A finite graph traversal limit was exceeded.
    Limit(GraphLimitError),
}

impl fmt::Display for GraphCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingModuleSource => formatter.write_str(
                "surface graph check requires a module source or a synthetic root source",
            ),
            Self::Limit(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for GraphCheckError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MissingModuleSource => None,
            Self::Limit(error) => Some(error),
        }
    }
}

/// Options for checking one source module.
#[derive(Clone, Debug, Default)]
pub struct CheckOptions {
    config: Config,
    mode: Option<Mode>,
}

impl CheckOptions {
    /// Uses an explicit checker configuration.
    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Overrides the source analysis mode.
    #[must_use]
    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = Some(mode);
        self
    }
}

/// Root input for one surface graph check.
#[derive(Clone, Debug)]
pub enum GraphRoot<'source> {
    /// A root that the surface's module source must provide.
    Existing(ModuleName),
    /// A caller-supplied root overlaid on the surface's module source.
    Overlay(&'source Source),
}

impl GraphRoot<'_> {
    /// Creates an existing module-source root.
    #[must_use]
    pub fn existing(root: impl Into<ModuleName>) -> Self {
        Self::Existing(root.into())
    }

    /// Creates a caller-supplied overlay root.
    #[must_use]
    pub const fn overlay(source: &Source) -> GraphRoot<'_> {
        GraphRoot::Overlay(source)
    }
}

/// Options for checking a source graph.
#[derive(Clone, Copy, Debug)]
pub struct GraphCheckOptions {
    parse: ParseConfig,
    root_mode: Option<Mode>,
    limits: GraphLimits,
}

impl Default for GraphCheckOptions {
    fn default() -> Self {
        Self {
            parse: Config::default().parse,
            root_mode: None,
            limits: GraphLimits::default(),
        }
    }
}

impl GraphCheckOptions {
    /// Replaces the parser configuration used for graph modules.
    #[must_use]
    pub fn with_parse_config(mut self, config: ParseConfig) -> Self {
        self.parse = config;
        self
    }

    /// Overrides the root source analysis mode.
    #[must_use]
    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.root_mode = Some(mode);
        self
    }

    /// Replaces the finite graph traversal limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: GraphLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Returns the finite graph traversal limits.
    #[must_use]
    pub const fn limits(&self) -> GraphLimits {
        self.limits
    }
}

/// Named VM execution policy for a [`Surface`]-built VM.
///
/// This groups the construction-time ambient environment, VM default limits,
/// sandbox policy, and source-module export policy.
#[derive(Clone, Debug)]
pub struct VmConfig {
    ambient: Ambient,
    limits: Limits,
    sandbox_policy: VmSandboxPolicy,
    source_module_export_policy: SourceModuleExportPolicy,
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
            source_module_export_policy: SourceModuleExportPolicy::Mutable,
        }
    }

    /// Builds a trusted host/internal VM configuration without installing the
    /// untrusted-code sandbox.
    #[must_use]
    pub fn trusted_host(ambient: Ambient, limits: Limits) -> Self {
        Self {
            ambient,
            limits,
            sandbox_policy: VmSandboxPolicy::TrustedHost,
            source_module_export_policy: SourceModuleExportPolicy::Mutable,
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

    /// Returns the source-module export policy applied by `require`.
    #[must_use]
    pub const fn source_module_export_policy(&self) -> SourceModuleExportPolicy {
        self.source_module_export_policy
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

    /// Replaces the source-module export policy.
    #[must_use]
    pub const fn with_source_module_export_policy(
        mut self,
        policy: SourceModuleExportPolicy,
    ) -> Self {
        self.source_module_export_policy = policy;
        self
    }
}

/// A validated runtime and checker surface.
#[derive(Clone)]
pub struct Surface {
    runtime_capabilities: RuntimeCapabilities,
    analysis_mode: Mode,
    modules: Vec<Arc<dyn NativeModule>>,
    module_declarations: Vec<DefinitionModule>,
    host_module_manifest_version: u64,
    module_source: Option<Arc<dyn SourceProvider>>,
    module_source_identity: Option<u64>,
    typed_module_entries: Arc<[TypedModuleEntry]>,
    declaration_globals: Vec<DeclarationGlobalSpec>,
    /// Lazily-built checker base shared by every clone of this surface: the
    /// builtin type environment, native require returns, and arena are
    /// constructed once and forked (cloned) per request, instead of rebuilt
    /// from declarations each time.
    checker_base: Arc<OnceLock<SurfaceCheckerBase>>,
    cache_fingerprint: Arc<OnceLock<[u8; 32]>>,
    /// Required exports replayed onto every [`Self::new_checker`] checker,
    /// validated against this surface's declared types at registration.
    required_globals: Vec<RequiredGlobalSpec>,
    /// Required type for the root module's single returned value.
    required_return: Option<String>,
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
    builtins: Environment,
    ambient_require_returns: Vec<(String, TypeId)>,
}

struct SurfaceModuleSources {
    host: Option<Arc<dyn SourceProvider>>,
    typed: Vec<TypedModuleEntry>,
}

static NEXT_MODULE_SOURCE_IDENTITY: AtomicU64 = AtomicU64::new(1);

fn next_module_source_identity() -> u64 {
    NEXT_MODULE_SOURCE_IDENTITY
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .expect("module source identity space exhausted")
}

fn update_fingerprint_field(hasher: &mut blake3::Hasher, field: &[u8]) {
    hasher.update(&field.len().to_le_bytes());
    hasher.update(field);
}

impl DeclarationGlobalSpec {
    pub(crate) fn source(&self) -> String {
        format!("declare {}: {}", self.name, self.type_text)
    }

    pub(crate) fn definition_module(&self) -> DefinitionModule {
        DefinitionModule::new(format!("<host-global:{}>", self.name), self.source())
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
    pub fn builder() -> Builder {
        Builder {
            libraries: Library::ALL.to_vec(),
            runtime_compilation: false,
            analysis_mode: Mode::Strict,
            modules: Vec::new(),
            typed_modules: Vec::new(),
            module_declarations: Vec::new(),
            module_source: None,
            declaration_globals: Vec::new(),
            required_globals: Vec::new(),
            required_return: None,
        }
    }

    fn from_validated_parts(
        runtime_capabilities: RuntimeCapabilities,
        analysis_mode: Mode,
        modules: Vec<Arc<dyn NativeModule>>,
        module_sources: SurfaceModuleSources,
        mut module_declarations: Vec<DefinitionModule>,
        declaration_globals: Vec<DeclarationGlobalSpec>,
        checker_base: Option<(Arena, Environment)>,
    ) -> Result<Self, ConfigError> {
        for global in &declaration_globals {
            module_declarations.push(global.definition_module());
        }
        let host_module_manifest_version =
            audit::host_module_manifest_version(&modules, &module_declarations);
        let typed_module_entries: Arc<[TypedModuleEntry]> = module_sources.typed.into();
        let module_source = Self::compose_module_source(&typed_module_entries, module_sources.host);
        let module_source_identity = module_source
            .as_ref()
            .map(|_| next_module_source_identity());
        let surface = Self {
            runtime_capabilities,
            analysis_mode,
            modules,
            module_declarations,
            host_module_manifest_version,
            module_source,
            module_source_identity,
            typed_module_entries,
            checker_base: Arc::new(OnceLock::new()),
            cache_fingerprint: Arc::new(OnceLock::new()),
            required_globals: Vec::new(),
            required_return: None,
            declaration_globals,
        };
        if let Some((arena, builtins)) = checker_base {
            let installed = surface
                .checker_base
                .set(surface.checker_base_from_builtins(arena, builtins));
            assert!(installed.is_ok(), "new surface checker base is empty");
        }
        Ok(surface)
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
    pub const fn analysis_mode(&self) -> Mode {
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

    /// Returns a stable fingerprint of every static field that affects source
    /// checking or bytecode compilation.
    ///
    /// Dynamic module-source state is represented separately by
    /// [`Self::module_source_cache_stamp`].
    #[doc(hidden)]
    #[must_use]
    pub fn cache_fingerprint(&self) -> [u8; 32] {
        *self.cache_fingerprint.get_or_init(|| {
            let mut hasher = blake3::Hasher::new();
            for library in self.libraries() {
                update_fingerprint_field(&mut hasher, library.global_name_bytes());
            }
            update_fingerprint_field(&mut hasher, &[u8::from(self.runtime_compilation_enabled())]);
            let mode = match self.analysis_mode {
                Mode::NoCheck => 0,
                Mode::Nonstrict => 1,
                Mode::Strict => 2,
            };
            update_fingerprint_field(&mut hasher, &[mode]);
            update_fingerprint_field(
                &mut hasher,
                &self.host_module_manifest_version.to_le_bytes(),
            );
            for declaration in &self.module_declarations {
                update_fingerprint_field(&mut hasher, declaration.name.as_bytes());
                update_fingerprint_field(&mut hasher, declaration.source.as_bytes());
                update_fingerprint_field(
                    &mut hasher,
                    &[match declaration.type_scope {
                        TypeScope::Ambient => 0,
                        TypeScope::Module => 1,
                    }],
                );
            }
            for required in &self.required_globals {
                update_fingerprint_field(&mut hasher, required.name.as_bytes());
                update_fingerprint_field(&mut hasher, required.type_text.as_bytes());
            }
            if let Some(required) = &self.required_return {
                update_fingerprint_field(&mut hasher, &[1]);
                update_fingerprint_field(&mut hasher, required.as_bytes());
            } else {
                update_fingerprint_field(&mut hasher, &[0]);
            }
            update_fingerprint_field(&mut hasher, &[u8::from(self.has_module_source())]);
            *hasher.finalize().as_bytes()
        })
    }

    /// Returns the process-unique provider identity and current epoch used by
    /// compilation caches, when this surface grants a module source.
    #[doc(hidden)]
    #[must_use]
    pub fn module_source_cache_stamp(&self) -> Option<(u64, u64)> {
        self.module_source.as_ref().map(|source| {
            (
                self.module_source_identity
                    .expect("a module source always has an identity"),
                source.epoch(),
            )
        })
    }

    /// Whether this surface grants `require` by installing a module source.
    #[must_use]
    pub fn has_module_source(&self) -> bool {
        self.module_source.is_some()
    }

    /// Builds the type-checker builtin environment for this surface.
    #[must_use]
    pub fn builtin_environment(&self, arena: &mut Arena) -> Environment {
        self.builtin_environment_with_require_returns(arena).0
    }

    fn checker_base(&self) -> SurfaceCheckerBase {
        let mut arena = Arena::new();
        let builtins = builtin_environment_for_with_definition_modules(
            self.runtime_capabilities(),
            &mut arena,
            self.declaration_modules(),
        )
        .expect("declaration modules were validated when the surface was built");
        self.checker_base_from_builtins(arena, builtins)
    }

    fn checker_base_from_builtins(
        &self,
        arena: Arena,
        builtins: Environment,
    ) -> SurfaceCheckerBase {
        let (builtins, ambient_require_returns) = self.configure_checker_base_builtins(builtins);
        SurfaceCheckerBase {
            arena,
            builtins,
            ambient_require_returns,
        }
    }

    fn builtin_environment_with_require_returns(
        &self,
        arena: &mut Arena,
    ) -> (Environment, Vec<(String, TypeId)>) {
        let builtins = builtin_environment_for_with_definition_modules(
            self.runtime_capabilities(),
            arena,
            self.declaration_modules(),
        )
        .expect("declaration modules were validated when the surface was built");
        let (builtins, ambient_require_returns) = self.configure_checker_base_builtins(builtins);
        (
            self.configure_module_source_builtins(builtins, !ambient_require_returns.is_empty()),
            ambient_require_returns,
        )
    }

    fn configure_checker_base_builtins(
        &self,
        mut builtins: Environment,
    ) -> (Environment, Vec<(String, TypeId)>) {
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
        let require_only_globals = self
            .modules
            .iter()
            .filter(|module| matches!(module.export(), ModuleExport::Require))
            .map(|module| module.name())
            .collect::<Vec<_>>();
        builtins = builtins.without_globals(require_only_globals);
        (builtins, ambient_require_returns)
    }

    fn configure_module_source_builtins(
        &self,
        builtins: Environment,
        has_ambient_require_returns: bool,
    ) -> Environment {
        if self.has_module_source() || has_ambient_require_returns {
            builtins
        } else {
            builtins.without_globals(["require"])
        }
    }

    /// Builds a checker session for this surface.
    #[must_use]
    pub fn new_checker(&self) -> Checker {
        let base = self.checker_base.get_or_init(|| self.checker_base());
        let builtins = self.configure_module_source_builtins(
            base.builtins.clone(),
            !base.ambient_require_returns.is_empty(),
        );
        let mut checker = Checker::with_builtins(base.arena.clone(), builtins);
        for (module, ty) in &base.ambient_require_returns {
            checker.define_require_return(module.as_str(), *ty);
        }
        for required in &self.required_globals {
            checker
                .require_global(&required.name, &required.type_text)
                .expect("required globals are validated when registered");
        }
        if let Some(required) = &self.required_return {
            checker
                .require_return(required)
                .expect("required return is validated when registered");
        }
        checker
    }

    /// Checks a named source using this surface's builtin environment.
    ///
    /// UTF-8 sources use the text checker path; byte-exact sources with
    /// invalid UTF-8 use the byte checker path.
    #[must_use]
    pub fn check(&self, source: &Source, options: CheckOptions) -> CheckedModule {
        let mut config = self.surface_config(options.config);
        if let Some(mode) = options.mode {
            config.default_mode = mode;
            config.source_mode_override = Some(mode);
        }
        let mut checker = self.new_checker();
        if let Some(text) = source.as_str() {
            checker.check_source_with_config(text, config)
        } else {
            checker.check_source_bytes_with_config(source.as_bytes(), config)
        }
    }

    /// Checks a source graph through a ready-only source-provider bridge.
    ///
    /// # Errors
    /// Returns [`GraphCheckError`] when an existing root has no source provider
    /// or configured traversal limits are exceeded.
    pub fn check_graph_ready(
        &self,
        root: GraphRoot<'_>,
        options: GraphCheckOptions,
    ) -> Result<CheckedGraph, GraphCheckError> {
        match root {
            GraphRoot::Existing(root) => {
                let Some(source) = self.module_source() else {
                    return Err(GraphCheckError::MissingModuleSource);
                };
                let mut frontend = self.configured_graph_checker(source.as_ref(), &options);
                frontend
                    .check_graph_blocking(root)
                    .map_err(GraphCheckError::Limit)
            }
            GraphRoot::Overlay(root) => {
                let source = Arc::new(self.root_overlay_source(root));
                let root = source.root_name();
                let mut frontend = self.configured_graph_checker(source.as_ref(), &options);
                frontend
                    .check_graph_blocking(root)
                    .map_err(GraphCheckError::Limit)
            }
        }
    }

    /// Checks a source graph and awaits asynchronous source-provider work.
    ///
    /// # Errors
    /// Returns [`GraphCheckError`] when an existing root has no source provider
    /// or configured traversal limits are exceeded.
    pub async fn check_graph(
        &self,
        root: GraphRoot<'_>,
        options: GraphCheckOptions,
    ) -> Result<CheckedGraph, GraphCheckError> {
        match root {
            GraphRoot::Existing(root) => {
                let Some(source) = self.module_source() else {
                    return Err(GraphCheckError::MissingModuleSource);
                };
                let mut frontend = self.configured_graph_checker(source.as_ref(), &options);
                frontend
                    .check_graph(root)
                    .await
                    .map_err(GraphCheckError::Limit)
            }
            GraphRoot::Overlay(root) => {
                let source = Arc::new(self.root_overlay_source(root));
                let root = source.root_name();
                let mut frontend = self.configured_graph_checker(source.as_ref(), &options);
                frontend
                    .check_graph(root)
                    .await
                    .map_err(GraphCheckError::Limit)
            }
        }
    }

    fn graph_checker<'source>(&self, source: &'source dyn SourceProvider) -> GraphChecker<'source> {
        let mut frontend =
            GraphChecker::with_checker(source, &EMPTY_CONFIG_RESOLVER, self.new_checker());
        frontend.set_source_mode_override(Some(self.analysis_mode()));
        frontend
    }

    fn configured_graph_checker<'source>(
        &self,
        source: &'source dyn SourceProvider,
        options: &GraphCheckOptions,
    ) -> GraphChecker<'source> {
        let mut frontend = self.graph_checker(source);
        frontend.set_graph_limits(Some(options.limits));
        frontend.set_parse_config(options.parse);
        frontend.set_root_mode_override(options.root_mode);
        frontend
    }

    fn root_overlay_source(&self, source: &Source) -> RootSource {
        let mut overlay = RootSource::new(source.id().clone(), source.as_bytes().to_vec())
            .with_display_name(source.display_name().to_owned())
            .with_root_requester(source.id().clone());
        if let Some(module_source) = self.module_source() {
            overlay = overlay.with_delegate(module_source);
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
        self.cache_fingerprint = Arc::new(OnceLock::new());
        Ok(())
    }

    /// Requires checked roots to return exactly one value of `type_text`.
    ///
    /// # Errors
    /// Returns [`ConfigError::InvalidRequiredReturn`] when the type cannot be
    /// resolved against this surface's declaration environment.
    pub fn require_return(&mut self, type_text: &str) -> Result<(), ConfigError> {
        let mut probe = self.new_checker();
        probe.require_return(type_text).map_err(|diagnostics| {
            ConfigError::InvalidRequiredReturn {
                reason: diagnostics.render("<required-return>"),
            }
        })?;
        self.required_return = Some(type_text.to_owned());
        self.cache_fingerprint = Arc::new(OnceLock::new());
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
            .runtime_capabilities(self.runtime_capabilities().clone())
            .source_module_export_policy(config.source_module_export_policy());
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

    /// Compiles a named source under this surface's runtime capabilities.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile(
        &self,
        source: &Source,
        options: &CompileOptions,
    ) -> Result<BytecodeChunk, CompileError> {
        self.runtime_capabilities()
            .compile_source(source.as_bytes(), options)
    }

    /// Compiles an existing shared parse product under this surface's runtime
    /// capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`CompileError`] for malformed source, incompatible parser
    /// options, compiler limits, or cancellation.
    #[doc(hidden)]
    pub fn compile_parsed(
        &self,
        parsed: &ParsedModule,
        options: &CompileOptions,
    ) -> Result<BytecodeChunk, CompileError> {
        self.runtime_capabilities()
            .compile_parsed_module_with_cancel(parsed, options, None)
    }

    /// Compiles and validates a named source into a
    /// [`CompiledModule`](ruau_vm::CompiledModule).
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_module(
        &self,
        source: &Source,
        options: &CompileOptions,
    ) -> Result<ruau_vm::CompiledModule, CompileError> {
        self.runtime_capabilities()
            .compile_module(source.as_bytes(), options)
    }

    /// The optional `require` source this surface grants.
    #[must_use]
    pub fn module_source(&self) -> Option<Arc<dyn SourceProvider>> {
        self.module_source.clone()
    }

    /// Returns this surface with `source` installed as its runtime `require`
    /// source.
    #[must_use]
    pub fn with_module_source(mut self, source: Arc<dyn SourceProvider>) -> Self {
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
    pub fn replace_module_source(&mut self, source: Option<Arc<dyn SourceProvider>>) {
        self.module_source = Self::compose_module_source(&self.typed_module_entries, source);
        self.module_source_identity = self
            .module_source
            .as_ref()
            .map(|_| next_module_source_identity());
        self.cache_fingerprint = Arc::new(OnceLock::new());
    }

    /// Wraps and installs the surface's fully composed module source in a
    /// snapshot provider.
    ///
    /// Typed-module fallbacks remain beneath the snapshot, so their reads and
    /// resolution edges participate in graph sealing.
    #[must_use]
    pub fn snapshot_module_source(&mut self) -> Option<Arc<SnapshotSource>> {
        let source = self.module_source.take()?;
        let snapshot = Arc::new(SnapshotSource::new(source));
        self.module_source = Some(snapshot.clone());
        self.module_source_identity = Some(next_module_source_identity());
        Some(snapshot)
    }

    fn compose_module_source(
        entries: &[TypedModuleEntry],
        host: Option<Arc<dyn SourceProvider>>,
    ) -> Option<Arc<dyn SourceProvider>> {
        if entries.is_empty() {
            host
        } else {
            Some(Arc::new(TypedModuleSource::new(entries, host)))
        }
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
            .field("required_return", &self.required_return)
            .field("declaration_globals", &self.declaration_globals)
            .finish()
    }
}

/// Builder for a [`Surface`].
pub struct Builder {
    libraries: Vec<Library>,
    runtime_compilation: bool,
    analysis_mode: Mode,
    modules: Vec<Arc<dyn NativeModule>>,
    typed_modules: Vec<TypedModuleSpec>,
    module_declarations: Vec<DefinitionModule>,
    module_source: Option<Arc<dyn SourceProvider>>,
    declaration_globals: Vec<DeclarationGlobalSpec>,
    required_globals: Vec<RequiredGlobalSpec>,
    required_return: Option<String>,
}

/// A typed native module's require identity.
struct TypedModuleSpec {
    /// The native module's name.
    name: String,
    /// Optional public require declaration distinct from the runtime audit declaration.
    declaration: Option<String>,
    /// Require aliases that resolve to the module.
    aliases: Vec<String>,
}

impl Builder {
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
    pub fn analysis_mode(mut self, mode: Mode) -> Self {
        self.analysis_mode = mode;
        self
    }

    /// Grants one audited native module to the surface.
    #[must_use]
    pub fn module(mut self, module: Arc<dyn NativeModule>) -> Self {
        self.modules.push(module);
        self
    }

    /// Grants one audited native module whose module API is also its typed
    /// require target.
    ///
    /// The module's declaration becomes a module-scoped definition module, so
    /// its type names stay private to the module. The declaration-erased
    /// source becomes the require target under the module's name, and each
    /// alias resolves to it. Importers reach the module's exported types
    /// through the import, as `local http = require(...)` then `http.Response`.
    ///
    /// The declaration must end with a top-level `return <global>`, and the
    /// module must export globals, because the erased source returns the
    /// module's global.
    #[must_use]
    pub fn typed_module<I, S>(mut self, module: Arc<dyn NativeModule>, aliases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.typed_modules.push(TypedModuleSpec {
            name: module.name().to_owned(),
            declaration: None,
            aliases: aliases.into_iter().map(Into::into).collect(),
        });
        self.modules.push(module);
        self
    }

    /// Grants an audited native module with a separate typed require declaration.
    ///
    /// Use this when the public declaration imports another typed module. The native module's
    /// own declaration remains the authoritative runtime-binding audit, while `declaration`
    /// becomes the module-scoped require source checked in the normal module graph. This keeps
    /// imported type names out of the ambient checker environment and gives missing or cyclic
    /// imports the same diagnostics as source modules.
    ///
    /// The public declaration must end with a top-level `return`. The native module must export
    /// globals, because the declaration-erased source returns its installed global value.
    #[must_use]
    pub fn typed_module_with_declaration<I, S>(
        mut self,
        module: Arc<dyn NativeModule>,
        declaration: decl::DeclarationSource<'_>,
        aliases: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.typed_modules.push(TypedModuleSpec {
            name: module.name().to_owned(),
            declaration: Some(declaration.render().into_owned()),
            aliases: aliases.into_iter().map(Into::into).collect(),
        });
        self.modules.push(module);
        self
    }

    /// Adds a checker-only declaration module.
    ///
    /// The declaration contributes global and type names to this surface's
    /// checker environment, but installs no VM bindings. Use this for static
    /// host API shapes that are checked separately from the runtime module
    /// implementation.
    #[must_use]
    pub fn declaration_module(mut self, name: &str, source: decl::DeclarationSource<'_>) -> Self {
        self.module_declarations.push(DefinitionModule::new(
            name.to_owned(),
            source.render().into_owned(),
        ));
        self
    }

    /// Grants runtime `require` through the supplied module source.
    #[must_use]
    pub fn module_source(mut self, source: Arc<dyn SourceProvider>) -> Self {
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
    pub fn declaration_global_ty(mut self, name: &str, ty: &decl::Type) -> Self {
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

    /// Requires the checked root module to return exactly one value of `type_text`.
    #[must_use]
    pub fn require_return(mut self, type_text: &str) -> Self {
        self.required_return = Some(type_text.to_owned());
        self
    }

    /// Builds the require-source entries for the registered typed modules.
    fn typed_module_entries(
        &self,
        host_module_audit: &audit::HostModuleAudit,
    ) -> Result<Vec<TypedModuleEntry>, ConfigError> {
        let mut seen = BTreeSet::new();
        let mut entries = Vec::with_capacity(self.typed_modules.len());
        for spec in &self.typed_modules {
            if !seen.insert(spec.name.as_str()) {
                return Err(ConfigError::InvalidHostModuleDeclaration {
                    module: spec.name.clone(),
                    reason: "duplicate typed module name".to_owned(),
                });
            }
            let module = self
                .modules
                .iter()
                .find(|module| module.name() == spec.name)
                .expect("typed_module registers the native module");
            if !matches!(module.export(), ModuleExport::Globals) {
                return Err(ConfigError::InvalidHostModuleDeclaration {
                    module: spec.name.clone(),
                    reason: "a typed module must export globals; its erased source returns the \
                             module's global"
                        .to_owned(),
                });
            }
            let declaration = host_module_audit
                .declarations()
                .iter()
                .find(|declaration| declaration.name.as_ref() == spec.name)
                .expect("typed module declaration was audited");
            let require_declaration = spec
                .declaration
                .as_deref()
                .unwrap_or(declaration.source.as_ref());
            let erased = if spec.declaration.is_some() {
                typed_module_require_source(require_declaration).map_err(|reason| {
                    ConfigError::InvalidHostModuleDeclaration {
                        module: spec.name.clone(),
                        reason: format!("public require declaration is invalid: {reason}"),
                    }
                })?
            } else {
                erase_declarations(require_declaration, EraseDeclarationsOptions::default())
                    .map_err(|errors| ConfigError::InvalidHostModuleDeclaration {
                        module: spec.name.clone(),
                        reason: format!(
                            "declaration cannot become a require source: {}",
                            errors
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join("; ")
                        ),
                    })?
            };
            if !source_has_top_level_return(&erased) {
                return Err(ConfigError::InvalidHostModuleDeclaration {
                    module: spec.name.clone(),
                    reason: "a typed module declaration must end with a top-level `return`"
                        .to_owned(),
                });
            }
            entries.push(TypedModuleEntry {
                name: spec.name.clone(),
                source: erased,
                aliases: spec.aliases.clone(),
            });
        }
        let mut require_ids = BTreeSet::new();
        for entry in &entries {
            require_ids.insert(ruau_source::ModuleId::canonicalized(entry.name.as_str()));
        }
        for entry in &entries {
            for alias in &entry.aliases {
                let alias_id = ruau_source::ModuleId::canonicalized(alias.as_str());
                if !require_ids.insert(alias_id) {
                    return Err(ConfigError::InvalidHostModuleDeclaration {
                        module: entry.name.clone(),
                        reason: format!(
                            "typed module alias {alias} collides with another typed module id"
                        ),
                    });
                }
            }
        }
        Ok(entries)
    }

    /// Validates module declarations and returns the exact surface.
    ///
    /// # Errors
    /// Returns [`ConfigError`] if any host module declaration is malformed,
    /// mismatched, or tries to bind a surface-omitted library.
    pub fn build(self) -> Result<Surface, ConfigError> {
        let runtime_capabilities = if self.runtime_compilation {
            RuntimeCapabilities::from_libraries(self.libraries.clone()).enable_runtime_compilation()
        } else {
            RuntimeCapabilities::from_libraries(self.libraries.clone())
        };
        let host_module_audit = audit::validate_host_modules(&runtime_capabilities, &self.modules)?;
        let typed_module_entries = self.typed_module_entries(&host_module_audit)?;
        let typed_names: BTreeSet<&str> = self
            .typed_modules
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        let mut module_declarations = host_module_audit.declarations().to_vec();
        for declaration in &mut module_declarations {
            if typed_names.contains(declaration.name.as_ref()) {
                declaration.type_scope = TypeScope::Module;
            }
        }
        module_declarations.extend(self.module_declarations);
        let (arena, builtins) = audit::validate_declaration_modules(
            &runtime_capabilities,
            &module_declarations,
            &host_module_audit,
        )?;
        let checker_base = (arena, builtins);
        audit::validate_declaration_globals(
            &runtime_capabilities,
            &module_declarations,
            &self.declaration_globals,
        )?;
        let checker_base = self.declaration_globals.is_empty().then_some(checker_base);
        let mut surface = Surface::from_validated_parts(
            runtime_capabilities,
            self.analysis_mode,
            self.modules,
            SurfaceModuleSources {
                host: self.module_source,
                typed: typed_module_entries,
            },
            module_declarations,
            self.declaration_globals,
            checker_base,
        )?;
        for required in self.required_globals {
            surface.require_global(&required.name, &required.type_text)?;
        }
        if let Some(required) = self.required_return {
            surface.require_return(&required)?;
        }
        Ok(surface)
    }
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn check_mode_builder_preserves_the_supplied_config_in_both_orders() {
        let mut config = Config::default();
        config.parse.capture_comments = false;
        config.generation.primitive_inference_table_limit = 7;

        let config_then_mode = CheckOptions::default()
            .with_config(config.clone())
            .with_mode(Mode::Nonstrict);
        let mode_then_config = CheckOptions::default()
            .with_mode(Mode::Nonstrict)
            .with_config(config.clone());

        assert_eq!(config_then_mode.config, config);
        assert_eq!(mode_then_config.config, config);
        assert_eq!(config_then_mode.mode, Some(Mode::Nonstrict));
        assert_eq!(mode_then_config.mode, Some(Mode::Nonstrict));
    }
}
