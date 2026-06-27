//! Runtime and checker surface configuration.
//!
//! A [`SurfaceSpec`] combines a VM profile, native modules, declaration
//! modules, and optional `require` source.

// The front-door verdict cache exists for the native runner; wasm builds
// carry no runner, so the whole cluster is compiled out with it.
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use ruau_abi::{
    HostFunction, ModuleBinding, ModuleBuilder, ModuleExport, ModuleValue, NativeModule,
};
use ruau_ast::{
    parse::{Options, SyntaxFlags, parse_file_with},
    syntax::{Stat, TableProp, Type},
};
use ruau_bytecode::BytecodeChunk;
use ruau_decl as decl;
use ruau_source::ModuleSource;
use ruau_typecheck::{
    builtins::{BuiltinEnvironment, DefinitionModule},
    checker::{CheckedModule, Checker, Config, ConformanceCheck},
    types::{Arena, TypeId},
    views::TypeView,
};
use ruau_vm::{Ambient, HostType, Limits, Profile, VmBuilder};

#[cfg(not(target_arch = "wasm32"))]
use crate::typecheck::diagnostic::TypeDiagnostic;
use crate::{
    analysis::resolve::AnalysisMode, profile::builtin_environment_for_with_definition_modules,
};

/// Surface or runner configuration error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// No [`Profile`] was selected.
    MissingProfile,
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
    /// No host environment choice was selected.
    MissingHostEnvironment,
    /// A prebuilt surface was combined with direct surface fields.
    ConflictingSurfaceConfiguration,
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
    /// Runtime compilation was requested without a profile that installs
    /// `loadstring`.
    RuntimeCompilationFeatureWithoutProfile,
    /// A profile installs `loadstring`, but the matching feature was not enabled.
    RuntimeCompilationProfileWithoutFeature,
    /// A compatibility feature is not supported by this runner.
    UnsupportedFeature,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ruau configuration error: ")?;
        let reason = match self {
            Self::MissingProfile => "no production profile selected",
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
            Self::MissingHostEnvironment => "no host environment selected",
            Self::ConflictingSurfaceConfiguration => {
                "prebuilt surface cannot be combined with profile, module, or source settings"
            }
            Self::InvalidHostModuleDeclaration { module, reason } => {
                return write!(f, "host module {module} declaration is invalid: {reason}");
            }
            Self::InvalidRequiredGlobal { name, reason } => {
                return write!(f, "required global {name} is invalid: {reason}");
            }
            Self::InvalidDeclarationGlobal { name, reason } => {
                return write!(f, "declaration global {name} is invalid: {reason}");
            }
            Self::RuntimeCompilationFeatureWithoutProfile => {
                "runtime compilation feature enabled without a loadstring profile"
            }
            Self::RuntimeCompilationProfileWithoutFeature => {
                "loadstring profile selected without runtime compilation feature"
            }
            Self::UnsupportedFeature => {
                "a compatibility feature is enabled but not yet wired into the pipeline"
            }
        };
        f.write_str(reason)
    }
}

impl std::error::Error for ConfigError {}

/// What the front door learned about one source under one surface: either the
/// strict-mode verdict failed with diagnostics, or it passed and compiled to a
/// chunk. Metrics are the values measured on the original pass; cache hits
/// re-enforce the runner's front-door limits against them.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub(crate) struct FrontDoorVerdict {
    pub(crate) ast_nodes: usize,
    pub(crate) type_arena_nodes: usize,
    pub(crate) outcome: FrontDoorOutcome,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub(crate) enum FrontDoorOutcome {
    TypeErrors(Vec<TypeDiagnostic>),
    Chunk(Arc<BytecodeChunk>),
}

/// Bounded source-verdict cache: repeated sources skip the parse, check, and
/// compile stages entirely. Keyed by the blake3 hash of the source plus the
/// runner compile options (the surface identity is the cache's owner). Shared
/// across tenants by design — every cached value derives from the source
/// bytes and the surface alone.
#[cfg(not(target_arch = "wasm32"))]
const FRONT_DOOR_CACHE_ENTRIES: usize = 256;

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub(crate) struct FrontDoorCache {
    entries: std::sync::Mutex<FrontDoorCacheMap>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub(crate) struct FrontDoorCacheMap {
    map: HashMap<[u8; 32], FrontDoorVerdict>,
    order: std::collections::VecDeque<[u8; 32]>,
}

#[cfg(not(target_arch = "wasm32"))]
impl FrontDoorCache {
    pub(crate) fn get(&self, key: &[u8; 32]) -> Option<FrontDoorVerdict> {
        let inner = self.entries.lock().ok()?;
        inner.map.get(key).cloned()
    }

    pub(crate) fn insert(&self, key: [u8; 32], verdict: FrontDoorVerdict) {
        let Ok(mut inner) = self.entries.lock() else {
            return;
        };
        if inner.map.insert(key, verdict).is_none() {
            inner.order.push_back(key);
            while inner.order.len() > FRONT_DOOR_CACHE_ENTRIES {
                if let Some(evicted) = inner.order.pop_front() {
                    inner.map.remove(&evicted);
                }
            }
        }
    }
}

/// A validated runtime and checker surface.
#[derive(Clone)]
pub struct SurfaceSpec {
    profile: Profile,
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
    /// Source-verdict cache shared by every clone of this surface.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) front_door_cache: Arc<FrontDoorCache>,
    /// Required exports replayed onto every [`Self::new_checker`] checker,
    /// validated against this surface's declared types at registration.
    required_globals: Vec<RequiredGlobalSpec>,
}

/// One validated required-export obligation carried by a [`SurfaceSpec`].
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

impl SurfaceSpec {
    /// Starts a surface builder for `profile`.
    #[must_use]
    pub fn builder(profile: Profile) -> SurfaceSpecBuilder {
        SurfaceSpecBuilder {
            profile,
            analysis_mode: AnalysisMode::Strict,
            modules: Vec::new(),
            module_source: None,
            declaration_globals: Vec::new(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn from_parts(
        profile: Profile,
        modules: Vec<Arc<dyn NativeModule>>,
        module_source: Option<Arc<dyn ModuleSource>>,
    ) -> Result<Self, ConfigError> {
        Self::from_parts_with_analysis_mode(profile, AnalysisMode::Strict, modules, module_source)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn from_parts_with_analysis_mode(
        profile: Profile,
        analysis_mode: AnalysisMode,
        modules: Vec<Arc<dyn NativeModule>>,
        module_source: Option<Arc<dyn ModuleSource>>,
    ) -> Result<Self, ConfigError> {
        let module_declarations = validate_host_modules(&profile, &modules)?;
        let declaration_globals = Vec::new();
        Self::from_validated_parts(
            profile,
            analysis_mode,
            modules,
            module_source,
            module_declarations,
            declaration_globals,
        )
    }

    fn from_validated_parts(
        profile: Profile,
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
            profile,
            analysis_mode,
            modules,
            module_declarations,
            host_module_manifest_version,
            module_source,
            checker_base: Arc::new(std::sync::OnceLock::new()),
            #[cfg(not(target_arch = "wasm32"))]
            front_door_cache: Arc::new(FrontDoorCache::default()),
            required_globals: Vec::new(),
            declaration_globals,
        })
    }

    /// The selected VM profile.
    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
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
            self.profile(),
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

    /// Checks source bytes using this surface's environment and analysis mode.
    #[must_use]
    pub fn check_source_bytes(&self, source: &[u8]) -> CheckedModule {
        self.check_source_bytes_with_config(source, Config::default())
    }

    /// Checks source bytes using this surface's builtin environment and an
    /// explicit checker configuration.
    ///
    /// If `config` does not already force a source mode, this method fills the
    /// override from [`Self::analysis_mode`]. Caller-provided overrides win.
    #[must_use]
    pub fn check_source_bytes_with_config(
        &self,
        source: &[u8],
        mut config: Config,
    ) -> CheckedModule {
        if config.source_mode_override.is_none() {
            config.source_mode_override = Some(self.analysis_mode());
            config.default_mode = self.analysis_mode();
        }
        let mut checker = self.new_checker();
        checker.check_source_bytes_with_config(source, config)
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
                reason: crate::typecheck::diagnostic::render_diagnostic_summary(
                    "<required-export>",
                    &diagnostics,
                ),
            })?;
        self.required_globals.push(RequiredGlobalSpec {
            name: name.to_owned(),
            type_text: type_text.to_owned(),
        });
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.front_door_cache = Arc::new(FrontDoorCache::default());
        }
        Ok(())
    }

    /// Returns a [`VmBuilder`] configured with this surface's profile, native
    /// modules, and optional `require` source.
    #[must_use]
    pub fn vm_builder(&self, ambient: Ambient, limits: Limits) -> VmBuilder {
        let mut builder = ruau_vm::Vm::builder()
            .ambient(ambient)
            .limits(limits)
            .profile(*self.profile());
        if let Some(source) = self.module_source() {
            builder = builder.module_source(source);
        }
        for module in self.native_modules() {
            builder = builder.module(Arc::clone(module));
        }
        builder
    }

    /// Compiles `source` under this surface's profile.
    ///
    /// # Errors
    /// As [`compile_for`](crate::compile::compile_for).
    pub fn compile(
        &self,
        source: &[u8],
        base: &crate::compile::CompileOptions,
    ) -> Result<BytecodeChunk, crate::compile::CompileError> {
        crate::compile::compile_for(self.profile(), source, base)
    }

    /// Compiles and validates `source` into a [`CompiledModule`](crate::vm::CompiledModule).
    ///
    /// # Errors
    /// As [`compile_module_for`](crate::compile::compile_module_for).
    pub fn compile_module(
        &self,
        source: &[u8],
        base: &crate::compile::CompileOptions,
    ) -> Result<crate::vm::CompiledModule, crate::compile::CompileError> {
        crate::compile::compile_module_for(self.profile(), source, base)
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
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.front_door_cache = Arc::new(FrontDoorCache::default());
        }
    }
}

impl std::fmt::Debug for SurfaceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurfaceSpec")
            .field("profile", &self.profile)
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

/// Builder for a [`SurfaceSpec`].
pub struct SurfaceSpecBuilder {
    profile: Profile,
    analysis_mode: AnalysisMode,
    modules: Vec<Arc<dyn NativeModule>>,
    module_source: Option<Arc<dyn ModuleSource>>,
    declaration_globals: Vec<DeclarationGlobalSpec>,
}

impl SurfaceSpecBuilder {
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

    /// Validates module declarations and returns the exact surface.
    ///
    /// # Errors
    /// Returns [`ConfigError`] if any host module declaration is malformed,
    /// mismatched, or tries to bind a profile-omitted library.
    pub fn build(self) -> Result<SurfaceSpec, ConfigError> {
        let module_declarations = validate_host_modules(&self.profile, &self.modules)?;
        validate_declaration_globals(
            &self.profile,
            &module_declarations,
            &self.declaration_globals,
        )?;
        SurfaceSpec::from_validated_parts(
            self.profile,
            self.analysis_mode,
            self.modules,
            self.module_source,
            module_declarations,
            self.declaration_globals,
        )
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
            && let Err(error) = collect_module_export_value_shape(
                ShapeWalk {
                    module: &self.module,
                    root: &self.module,
                },
                &mut self.shape,
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
                        collect_module_value_shape(
                            ShapeWalk {
                                module: &self.module,
                                root: name,
                            },
                            &mut self.shape,
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
                    collect_module_value_shape(
                        ShapeWalk {
                            module: &self.module,
                            root: library.as_ref(),
                        },
                        &mut self.shape,
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

fn collect_module_value_shape(
    walk: ShapeWalk<'_>,
    shape: &mut HostModuleShape,
    prefix: &str,
    value: &ModuleValue,
) -> Result<(), String> {
    let ModuleValue::Table(table) = value else {
        return Ok(());
    };
    for entry in &table.entries {
        let path = walk.member_path(prefix, entry.name.as_ref());
        let kind = module_value_kind(&entry.value);
        shape.insert_library_member(walk.module, walk.root, &path, kind)?;
        collect_module_value_shape(walk, shape, &path, &entry.value)?;
    }
    Ok(())
}

fn collect_module_export_value_shape(
    walk: ShapeWalk<'_>,
    shape: &mut HostModuleShape,
    prefix: &str,
    value: &ModuleValue,
) -> Result<(), String> {
    let ModuleValue::Table(table) = value else {
        return Ok(());
    };
    for entry in &table.entries {
        let path = walk.member_path(prefix, entry.name.as_ref());
        let kind = module_value_kind(&entry.value);
        shape.insert_module_export_member(walk.module, &path, kind)?;
        collect_module_export_value_shape(walk, shape, &path, &entry.value)?;
    }
    Ok(())
}

fn validate_host_modules(
    profile: &Profile,
    modules: &[Arc<dyn NativeModule>],
) -> Result<Vec<DefinitionModule>, ConfigError> {
    let mut declarations = Vec::with_capacity(modules.len());
    let mut shapes = Vec::with_capacity(modules.len());
    let mut all_bindings = HostModuleShape::default();
    let builtin_globals = profile_builtin_global_names(profile);
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
        reject_profile_omitted_host_bindings(profile, module.name(), &runtime)?;
        reject_unflagged_builtin_collisions(&builtin_globals, module.name(), &runtime)?;
        merge_host_module_shape(&mut all_bindings, module.name(), &runtime)?;
        declarations.push(DefinitionModule {
            name: module.name().to_owned().into(),
            source: declaration.into_owned().into(),
        });
        shapes.push((module.name().to_owned(), expected));
    }
    validate_host_module_declaration_types(profile, &declarations, &shapes)?;
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

/// The builtin global names the checker environment defines for `profile`,
/// before any host-module declaration is merged — the collision reference for
/// global bindings.
fn profile_builtin_global_names(profile: &Profile) -> BTreeSet<String> {
    let mut arena = Arena::new();
    crate::profile::builtin_environment_for(profile, &mut arena)
        .globals()
        .map(|global| global.name.clone())
        .collect()
}

/// Global bindings are fail-closed about the profile's builtin surface: a
/// plain `Global` colliding with a builtin requires the explicit
/// `GlobalOverride` opt-in, and an override must have a builtin to replace.
fn reject_unflagged_builtin_collisions(
    builtin_globals: &BTreeSet<String>,
    module: &str,
    shape: &HostModuleShape,
) -> Result<(), ConfigError> {
    for global in shape.globals.keys() {
        // A library root shared with a profile library (a module extending
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
                    "global {global} collides with a profile builtin; replacing it \
                     requires the explicit ModuleBinding::GlobalOverride opt-in"
                ),
            });
        }
        if overrides && !collides {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!(
                    "global {global} is bound as an override, but the profile \
                     installs no builtin of that name to replace"
                ),
            });
        }
    }
    Ok(())
}

fn reject_profile_omitted_host_bindings(
    profile: &Profile,
    module: &str,
    shape: &HostModuleShape,
) -> Result<(), ConfigError> {
    for library in profile.omitted_libraries() {
        let name = library.global_name();
        if shape.globals.contains_key(name) || shape.libraries.contains_key(name) {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!("binds omitted profile library {name}"),
            });
        }
    }
    Ok(())
}

fn merge_host_module_shape(
    target: &mut HostModuleShape,
    module: &str,
    shape: &HostModuleShape,
) -> Result<(), ConfigError> {
    for (global, kind) in &shape.globals {
        if target.globals.get(global) == Some(kind)
            && *kind == HostBindingKind::Table
            && target.library_roots.contains(global)
            && shape.library_roots.contains(global)
        {
            continue;
        }
        target
            .insert_global(module, global, *kind)
            .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason,
            })?;
        if shape.library_roots.contains(global) {
            target.library_roots.insert(global.clone());
        }
    }
    for (library, members) in &shape.libraries {
        for (member, kind) in members {
            target
                .insert_library_member(module, library, member, *kind)
                .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                    module: module.to_owned(),
                    reason,
                })?;
        }
    }
    for (export, members) in &shape.module_exports {
        if target.module_exports.contains_key(export) {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!("duplicate native require export {export}"),
            });
        }
        for (member, kind) in members {
            target
                .insert_module_export_member(module, member, *kind)
                .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                    module: module.to_owned(),
                    reason,
                })?;
        }
    }
    for (table, members) in &shape.hidden {
        if target.support_chunks.contains(table) {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!("hidden table {table} collides with a support chunk"),
            });
        }
        for (member, kind) in members {
            target
                .insert_hidden_member(module, table, member, *kind)
                .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                    module: module.to_owned(),
                    reason,
                })?;
        }
    }
    for key in &shape.support_chunks {
        target.insert_support_chunk(module, key).map_err(|reason| {
            ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason,
            }
        })?;
    }
    for host_type in &shape.host_types {
        target
            .insert_host_type(module, host_type)
            .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason,
            })?;
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
    profile: &Profile,
    declarations: &[DefinitionModule],
    shapes: &[(String, HostModuleShape)],
) -> Result<(), ConfigError> {
    let mut arena = Arena::new();
    let builtins =
        builtin_environment_for_with_definition_modules(profile, &mut arena, declarations);
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
    profile: &Profile,
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
        builtin_environment_for_with_definition_modules(profile, &mut arena, &declarations);
    let mut checker = Checker::with_builtins(arena, builtins);
    for global in globals {
        checker
            .require_global(&global.name, &global.type_text)
            .map_err(|diagnostics| ConfigError::InvalidDeclarationGlobal {
                name: global.name.clone(),
                reason: crate::typecheck::diagnostic::render_diagnostic_summary(
                    "<declaration-global>",
                    &diagnostics,
                ),
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
                collect_declared_table_shape(
                    ShapeWalk {
                        module,
                        root: name.as_str(),
                    },
                    shape,
                    "",
                    props,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_declared_table_shape(
    walk: ShapeWalk<'_>,
    shape: &mut HostModuleShape,
    prefix: &str,
    props: &[TableProp],
) -> Result<(), String> {
    for prop in props {
        let path = walk.member_path(prefix, prop.name.as_str());
        let kind = type_binding_kind(&prop.prop_type);
        shape.insert_library_member(walk.module, walk.root, &path, kind)?;
        if let Some(props) = table_props(&prop.prop_type) {
            collect_declared_table_shape(walk, shape, &path, props)?;
        }
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
