//! Checker entry points and module-checking orchestration scaffolding.

#![allow(clippy::multiple_inherent_impl)]

mod conformance;
mod module_surface;
mod required_globals;
mod single_module;

use std::{collections::BTreeMap, sync::Arc};

use ruau_analysis::{AnalysisMode, resolve::config::AnalysisConfig};
use ruau_ast::{
    parse::{ParseConfig, parse_type_with},
    syntax::Stat,
};
use ruau_source::ModuleName;

use self::required_globals::RequiredGlobal;
use crate::{
    annotation::lower_type_annotation,
    builtins::BuiltinEnvironment,
    constraints::{Constraint, ConstraintSolveSummary},
    dfg::DataFlowGraph,
    diagnostics::{Diagnostic, Diagnostics},
    queries::Queries,
    scopes::{ScopeTree, TypeBindingKind},
    types::{Arena, TableAliasIdentity, TypeId},
};

/// Configuration for expression constraint generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerationConfig {
    /// Maximum primitive literal entries in a table literal before contextual
    /// singleton inference is suppressed for those primitive values.
    pub primitive_inference_table_limit: usize,
}

impl Default for GenerationConfig {
    fn default() -> Self {
        Self {
            primitive_inference_table_limit: usize::MAX,
        }
    }
}

/// Configuration for a single-module checker invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// Portable analysis config visible to checker stages.
    pub analysis: AnalysisConfig,
    /// AnalysisMode used when a source file does not contain a mode hot comment.
    pub default_mode: AnalysisMode,
    /// AnalysisMode that wins over source hot comments and config defaults.
    pub source_mode_override: Option<AnalysisMode>,
    /// Parser configuration for source-text entry points.
    pub parse: ParseConfig,
    /// Constraint-generation knobs for checker compatibility behaviour.
    pub generation: GenerationConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            analysis: AnalysisConfig::new(),
            default_mode: AnalysisMode::Strict,
            source_mode_override: None,
            parse: ParseConfig {
                allow_declaration_syntax: true,
                capture_comments: true,
                ..ParseConfig::default()
            },
            generation: GenerationConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredGlobalPolicy {
    Judge,
    Skip,
}

impl Config {
    /// Creates a checker config that forces every source to use `mode`.
    ///
    /// The override wins over source hot comments and analysis-config defaults.
    #[must_use]
    pub fn with_source_mode(mode: AnalysisMode) -> Self {
        Self {
            default_mode: mode,
            source_mode_override: Some(mode),
            ..Self::default()
        }
    }
}

/// One checked single-module result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckedModule {
    /// Shared, not owned: the root is immutable after parse, so the checked
    /// result holds an `Arc` and cloning a `CheckedModule` (or constructing
    /// one from an owned parse) never deep-copies the AST.
    root: Arc<Stat>,
    mode: AnalysisMode,
    config: AnalysisConfig,
    diagnostics: Diagnostics,
    scopes: ScopeTree,
    dfg: DataFlowGraph,
    queries: Queries,
    exports: ModuleExports,
    return_types: Vec<TypeId>,
    imported_modules: BTreeMap<ModuleName, ImportedModuleSummary>,
    constraints: Vec<Constraint>,
    solve_summary: Option<ConstraintSolveSummary>,
    /// Solved types of user-defined global functions (`function f() ... end`),
    /// keyed by name. These never enter `scopes.globals` — the generator tracks
    /// them in its own map — so a by-name query (`requireType("f")`) needs this
    /// to resolve them.
    global_defs: BTreeMap<String, TypeId>,
    /// Local type answers that intentionally differ from value-flow storage.
    /// Consulted only by by-name queries, so assignment and expression checking
    /// keep the DFG/local binding.
    query_local_types: BTreeMap<ruau_ast::syntax::LocalId, TypeId>,
}

/// Exported type surface for a checked module.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleExports {
    types: BTreeMap<String, ExportedType>,
}

impl ModuleExports {
    /// Returns exported type bindings keyed by exported name.
    #[must_use]
    pub const fn types(&self) -> &BTreeMap<String, ExportedType> {
        &self.types
    }

    /// Returns true when the module has no exported type bindings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

/// One exported type binding from a checked module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportedType {
    /// Source-visible export name.
    pub name: String,
    /// Source alias definition identity for nominal table alias results.
    pub alias_identity: Option<TableAliasIdentity>,
    /// Binding category retained from the checked scope.
    pub kind: ExportedTypeKind,
    /// Elaborated type target when available.
    pub ty: Option<crate::types::TypeId>,
    /// Alias body before elaboration, retained for module-summary consumers.
    pub alias: Option<ruau_ast::syntax::Type>,
    /// True when the alias body depends on generic type or pack parameters.
    pub alias_has_generics: bool,
    /// Ordered generic type parameters for source aliases.
    pub generics: Vec<GenericParameter>,
    /// Ordered generic type-pack parameters for source aliases.
    pub generic_packs: Vec<GenericPackParameter>,
}

/// One generic type parameter of an exported source type alias.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenericParameter {
    /// Parameter name as written in source.
    pub name: String,
    /// Source location of the parameter declaration, when available.
    pub location: Option<ruau_ast::Location>,
    /// Declared default type, when present.
    pub default_type: Option<ruau_ast::syntax::Type>,
}

/// One generic type-pack parameter of an exported source type alias.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenericPackParameter {
    /// Parameter name as written in source.
    pub name: String,
    /// Source location of the parameter declaration, when available.
    pub location: Option<ruau_ast::Location>,
    /// Declared default type pack, when present.
    pub default_type: Option<ruau_ast::syntax::TypePack>,
}

/// Public category of an exported type binding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExportedTypeKind {
    /// Exported type alias.
    TypeAlias,
    /// User-defined class/type declaration.
    Class,
    /// Declared external class/type.
    DeclaredClass,
    /// User-defined type function.
    TypeFunction,
}

impl ExportedTypeKind {
    /// Maps a scope-level type binding kind onto its exported category.
    ///
    /// Panics on non-exportable binding kinds (generic parameters and
    /// builtins), which never form an `ExportedType`.
    pub(crate) fn from_binding_kind(kind: TypeBindingKind) -> Self {
        match kind {
            TypeBindingKind::TypeAlias | TypeBindingKind::ExportedTypeAlias => Self::TypeAlias,
            TypeBindingKind::Class => Self::Class,
            TypeBindingKind::DeclaredClass => Self::DeclaredClass,
            TypeBindingKind::TypeFunction => Self::TypeFunction,
            TypeBindingKind::GenericParameter
            | TypeBindingKind::GenericPackParameter
            | TypeBindingKind::BuiltinType => {
                panic!("non-exportable type binding kind cannot form an ExportedType")
            }
        }
    }
}

/// Compact checked summary retained by modules that import this module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedModuleSummary {
    /// Whether the imported module produced diagnostics.
    pub has_issues: bool,
    /// Whether the imported module produced error-severity diagnostics.
    pub has_errors: bool,
    /// Exported type surface available to importers.
    pub exports: ModuleExports,
    /// Top-level module return values available to `require` callers.
    pub return_types: Vec<TypeId>,
}

/// Result of checking one implementation module against one declaration source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConformanceCheck {
    diagnostics: Diagnostics,
    fingerprint: ConformanceFingerprint,
}

impl ConformanceCheck {
    /// Creates a conformance report from its parts.
    #[must_use]
    pub(crate) fn new(diagnostics: Diagnostics, fingerprint: ConformanceFingerprint) -> Self {
        Self {
            diagnostics,
            fingerprint,
        }
    }

    /// Structured diagnostics produced by the conformance check.
    #[must_use]
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// Consumes the report, returning its diagnostics.
    #[must_use]
    pub fn into_diagnostics(self) -> Diagnostics {
        self.diagnostics
    }

    /// Stable fingerprint for this conformance input.
    ///
    /// For direct [`Checker`](crate::Checker) checks, the fingerprint covers
    /// the implementation source, declaration source, and checker
    /// configuration. For graph checks through
    /// [`GraphChecker`](crate::frontend::GraphChecker), it additionally covers
    /// the root module, dependency graph outcome, resolver diagnostics, and the
    /// public interface snapshots of checked dependencies. Persisted caches
    /// should pair this opaque value with the Ruau crate/version or another
    /// application-level schema key; the exact hash algorithm is not an
    /// interoperability format.
    #[must_use]
    pub const fn fingerprint(&self) -> ConformanceFingerprint {
        self.fingerprint
    }

    /// Returns true when no diagnostics were produced.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// Returns true when at least one diagnostic was produced.
    #[must_use]
    pub fn has_issues(&self) -> bool {
        self.diagnostics.has_issues()
    }

    /// Returns true when at least one error-severity diagnostic was produced.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.has_errors()
    }
}

/// Stable cache key for a declaration-conformance check.
///
/// The value is deterministic for the same Ruau checker version, inputs, and
/// configuration. It is intended as an opaque local cache key, not as a
/// cross-version wire format.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConformanceFingerprint(u64);

impl ConformanceFingerprint {
    /// Creates a fingerprint from an already-computed stable digest.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw digest value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl CheckedModule {
    /// Parsed module root used for this check.
    #[must_use]
    pub fn root(&self) -> &Stat {
        &self.root
    }

    /// Effective mode for the module.
    #[must_use]
    pub const fn mode(&self) -> AnalysisMode {
        self.mode
    }

    /// Effective portable analysis config.
    #[must_use]
    pub const fn config(&self) -> &AnalysisConfig {
        &self.config
    }

    /// Structured diagnostics produced while checking.
    #[must_use]
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// Returns true when any diagnostics were produced.
    #[must_use]
    pub fn has_issues(&self) -> bool {
        self.diagnostics.has_issues()
    }

    /// Returns true when any error-severity diagnostics were produced.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.has_errors()
    }

    /// Lexical scope tree produced for the module.
    #[must_use]
    pub(crate) const fn scopes(&self) -> &ScopeTree {
        &self.scopes
    }

    /// DCR data-flow graph produced for the module.
    #[must_use]
    #[cfg_attr(not(any()), allow(dead_code))]
    pub(crate) const fn dfg(&self) -> &DataFlowGraph {
        &self.dfg
    }

    /// Root-scope local bindings as display-ready type summaries.
    #[must_use]
    pub fn root_local_summaries(&self, arena: &Arena) -> Vec<(String, String)> {
        self.dfg
            .defs()
            .filter_map(|(_, def)| match &def.kind {
                crate::dfg::DefKind::Local { name, .. } if def.scope == self.scopes.root() => {
                    Some((name.clone(), arena.summary(def.ty)))
                }
                _ => None,
            })
            .collect()
    }

    /// Solved type of a user-defined global function by name, if one was
    /// defined. Backs by-name resolution of `function f() ... end` bindings,
    /// which the generator records outside `scopes.globals`.
    #[must_use]
    pub(crate) fn global_def(&self, name: &str) -> Option<TypeId> {
        self.global_defs.get(name).copied()
    }

    /// Query-only local type answer, if any.
    #[must_use]
    #[cfg_attr(not(any()), allow(dead_code))]
    pub(crate) fn query_local_type(&self, local: ruau_ast::syntax::LocalId) -> Option<TypeId> {
        self.query_local_types.get(&local).copied()
    }

    /// Actual and expected type query data collected by source range and
    /// syntax id.
    #[must_use]
    #[cfg(any())]
    pub const fn queries(&self) -> &Queries {
        &self.queries
    }

    /// Actual and expected type query data collected by source range and
    /// syntax id.
    #[must_use]
    #[cfg(not(any()))]
    #[allow(dead_code)]
    pub(crate) const fn queries(&self) -> &Queries {
        &self.queries
    }

    /// Exported type surface collected from the module root scope.
    #[must_use]
    pub const fn exports(&self) -> &ModuleExports {
        &self.exports
    }

    /// Top-level module return values.
    #[must_use]
    pub fn return_types(&self) -> &[TypeId] {
        &self.return_types
    }

    /// Checked summaries for imported modules in the same frontend build.
    #[must_use]
    pub const fn imported_modules(&self) -> &BTreeMap<ModuleName, ImportedModuleSummary> {
        &self.imported_modules
    }

    /// Returns a compact summary suitable for importers.
    #[must_use]
    pub(crate) fn import_summary(&self) -> ImportedModuleSummary {
        ImportedModuleSummary {
            has_issues: self.has_issues(),
            has_errors: self.has_errors(),
            exports: self.exports.clone(),
            return_types: self.return_types.clone(),
        }
    }

    /// Attaches imported module summaries collected by a checked frontend.
    pub(crate) fn set_imported_modules(
        &mut self,
        imported_modules: BTreeMap<ModuleName, ImportedModuleSummary>,
    ) {
        self.imported_modules = imported_modules;
    }

    /// Appends additional frontend-level diagnostics after module checking.
    pub(crate) fn extend_diagnostics(&mut self, diagnostics: impl IntoIterator<Item = Diagnostic>) {
        self.diagnostics.extend(diagnostics);
    }

    /// Constraints generated while checking this module.
    #[must_use]
    #[cfg(any())]
    pub(crate) fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    /// Summary from the latest constraint-solver pass, if it reached a normal
    /// fixed point or blocked point.
    #[must_use]
    #[cfg(any())]
    pub(crate) const fn solve_summary(&self) -> Option<&ConstraintSolveSummary> {
        self.solve_summary.as_ref()
    }
}

/// Session-local Luau type checker.
#[derive(Clone, Debug)]
pub struct Checker {
    arena: Arena,
    builtins: BuiltinEnvironment,
    next_standalone_alias_module: u64,
    /// Cooperative cancellation polled by the constraint solver.
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Required-export obligations registered through
    /// [`Self::require_global`], judged after module checks.
    required_globals: Vec<RequiredGlobal>,
    /// Ambient module exports returned by literal `require("<name>")` calls
    /// without requiring a source-graph module.
    ambient_require_returns: BTreeMap<ModuleName, TypeId>,
}

impl Checker {
    /// Creates a checker with a fresh session arena and the standard builtin
    /// environment.
    #[must_use]
    pub fn new() -> Self {
        let mut arena = Arena::new();
        let builtins = BuiltinEnvironment::standard(&mut arena);
        Self {
            arena,
            builtins,
            next_standalone_alias_module: 0,
            cancel: None,
            required_globals: Vec::new(),
            ambient_require_returns: BTreeMap::new(),
        }
    }

    /// Installs a cooperative cancellation flag polled during checks.
    pub fn set_cancel_flag(&mut self, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.cancel = Some(cancel);
    }

    /// Creates a checker from an explicit arena and builtin environment.
    ///
    /// The builtin environment must use type handles from the supplied arena.
    #[must_use]
    pub const fn with_builtins(arena: Arena, builtins: BuiltinEnvironment) -> Self {
        Self {
            arena,
            builtins,
            next_standalone_alias_module: 0,
            cancel: None,
            required_globals: Vec::new(),
            ambient_require_returns: BTreeMap::new(),
        }
    }

    /// Returns the checker-session type arena.
    #[must_use]
    pub const fn arena(&self) -> &Arena {
        &self.arena
    }

    /// Consumes the checker, returning its session arena.
    ///
    /// Lets fixture harnesses keep the arena alive after the `&mut self`
    /// borrow that produced a `CheckedModule` ends, so the module's `TypeId`
    /// handles stay resolvable (e.g. to render a queried type).
    #[must_use]
    #[cfg(any())]
    pub(crate) fn into_arena(self) -> Arena {
        self.arena
    }

    /// Mutably returns the checker-session type arena.
    pub(crate) const fn arena_mut(&mut self) -> &mut Arena {
        &mut self.arena
    }

    /// Returns the checker-session builtin environment.
    #[must_use]
    pub const fn builtins(&self) -> &BuiltinEnvironment {
        &self.builtins
    }

    /// Defines the type returned by a literal `require("<module>")` call when
    /// the module is supplied by the ambient host surface rather than by the
    /// source graph.
    pub fn define_require_return(&mut self, module: impl Into<ModuleName>, ty: TypeId) {
        self.ambient_require_returns.insert(module.into(), ty);
    }

    pub(crate) fn ambient_require_return(&self, module: &ModuleName) -> Option<TypeId> {
        self.ambient_require_returns.get(module).copied()
    }

    /// Parses and lowers a standalone Luau type annotation into this checker
    /// session's type arena.
    pub fn parse_type(&mut self, source: &str) -> Result<crate::types::TypeId, Diagnostics> {
        let (ty, _diagnostics) = self.lower_annotation_text(source)?;
        Ok(ty)
    }

    /// Parses and lowers a standalone type annotation against this checker's
    /// builtin environment, returning the lowered type together with the
    /// lowering diagnostics (e.g. unknown type names).
    fn lower_annotation_text(
        &mut self,
        source: &str,
    ) -> Result<(TypeId, Diagnostics), Diagnostics> {
        let parsed = parse_type_with(source, &ParseConfig::default());
        if !parsed.errors.is_empty() {
            return Err(parsed.errors.iter().map(Diagnostic::from).collect());
        }
        let parsed_type = parsed.root;

        let mut scopes = ScopeTree::new();
        let root_scope = scopes.root();
        self.builtins.install_into_scope(&mut scopes, root_scope);
        let root = empty_root();
        let dfg = DataFlowGraph::build(&root, &scopes, &mut self.arena);
        let (ty, diagnostics) = lower_type_annotation(
            &parsed_type,
            &scopes,
            &dfg,
            &mut self.arena,
            AnalysisMode::Strict,
        );
        Ok((ty, diagnostics))
    }
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

fn empty_root() -> Stat {
    Stat::Block {
        location: None,
        has_end: false,
        is_do: false,
        body: Vec::new(),
    }
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn source_mode_override_ignores_hot_comment_mode_downgrades() {
        let mut checker = Checker::new();

        let checked = checker.check_source_with_config(
            "--!nocheck\nlocal n: number = 'nope'",
            Config::with_source_mode(AnalysisMode::Strict),
        );

        assert_eq!(checked.mode(), AnalysisMode::Strict);
        assert!(
            checked.has_errors(),
            "forced strict mode should type-check despite --!nocheck"
        );
    }

    #[test]
    fn source_mode_helper_sets_override_and_default_mode() {
        let config = Config::with_source_mode(AnalysisMode::Nonstrict);

        assert_eq!(config.default_mode, AnalysisMode::Nonstrict);
        assert_eq!(config.source_mode_override, Some(AnalysisMode::Nonstrict));
    }

    #[test]
    fn nonstrict_type_diagnostics_are_issues_but_not_errors() {
        let mut checker = Checker::new();
        let mut config = Config::with_source_mode(AnalysisMode::Nonstrict);
        config.analysis.set_type_errors(false);

        let checked = checker.check_source_with_config("local value: number = 'warning'", config);

        assert!(checked.has_issues(), "{:?}", checked.diagnostics());
        assert!(!checked.has_errors(), "{:?}", checked.diagnostics());
        assert_eq!(checked.diagnostics().warning_count(), 1);
        assert_eq!(checked.diagnostics().error_count(), 0);
    }

    #[test]
    fn source_text_and_utf8_bytes_check_the_same_module() {
        let source = "--!strict\nlocal value: number = 40 + 2\nreturn value";
        let mut text_checker = Checker::new();
        let mut byte_checker = Checker::new();

        let text = text_checker.check_source(source);
        let bytes = byte_checker.check_source_bytes(source.as_bytes());

        assert_eq!(text.mode(), bytes.mode());
        assert_eq!(text.diagnostics(), bytes.diagnostics());
        assert_eq!(text.return_types().len(), bytes.return_types().len());
    }

    #[test]
    fn source_byte_checking_preserves_invalid_string_singletons() {
        let mut checker = Checker::new();

        let checked = checker.check_source_bytes(b"--!strict\nlocal s: \"\xe9\" = \"\xea\"");

        assert!(
            checked.has_errors(),
            "distinct invalid string bytes must not collapse through a lossy view"
        );
    }
}
