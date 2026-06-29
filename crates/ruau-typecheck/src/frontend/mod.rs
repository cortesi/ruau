//! Checked frontend wrapper over `ruau-analysis` source graphs.

use std::collections::{BTreeMap, BTreeSet};

#[cfg(any())]
use ruau_analysis::fixtures::FileResolver;
use ruau_analysis::{
    Frontend, ParseGraphResult, RequireCycle, SourceModule,
    resolve::{AnalysisMode, config::Resolver},
};
use ruau_ast::{
    parse::{Options, SyntaxFlags},
    syntax::{Expr, Name, Stat, SyntaxId, Type, TypePack, TypeParameter},
};
use ruau_source::{ModuleName, ModuleSource};

use crate::{
    checker::{
        CheckedModule, Checker, Config, ConformanceCheck, ConformanceFingerprint, ExportedType,
        ExportedTypeKind, ImportedModuleSummary,
    },
    diagnostics::{
        Diagnostic, DiagnosticCategory, DiagnosticLocation, Diagnostics, GraphDiagnostics,
        ModuleDiagnostic,
    },
    interface_snapshot::InterfaceSnapshot,
    scopes::{ScopeTree, TypeBinding, TypeBindingKind},
    types::{TableAliasIdentity, TypeId},
};

#[cfg(any())]
type PrepareModuleScope<'resolver> = dyn FnMut(&ModuleName, &mut ScopeTree) + Send + 'resolver;

#[derive(Clone, Debug)]
struct EnvironmentGlobalBinding {
    name: String,
    ty: Option<TypeId>,
}

#[derive(Clone, Debug)]
struct ImportedTypeBinding {
    name: String,
    display_name: Option<String>,
    alias_identity: Option<TableAliasIdentity>,
    kind: TypeBindingKind,
    ty: Option<TypeId>,
    alias: Option<Type>,
    alias_has_generics: bool,
    generic_names: Vec<String>,
    generic_locations: Vec<Option<ruau_ast::Location>>,
    generic_defaults: Vec<Option<Type>>,
    generic_pack_names: Vec<String>,
    generic_pack_locations: Vec<Option<ruau_ast::Location>>,
    generic_pack_defaults: Vec<Option<TypePack>>,
}

/// Source-graph frontend that checks strict modules and skips nocheck modules.
pub struct GraphChecker<'resolver> {
    frontend: Frontend<'resolver>,
    checker: Checker,
    checked_modules: BTreeMap<ModuleName, CheckedModule>,
    queued_modules: BTreeSet<ModuleName>,
    environments: BTreeMap<String, Vec<TypeBinding>>,
    environment_globals: BTreeMap<String, Vec<EnvironmentGlobalBinding>>,
    #[cfg(any())]
    prepare_module_scope: Option<Box<PrepareModuleScope<'resolver>>>,
    source_mode_override: Option<AnalysisMode>,
}

impl<'resolver> GraphChecker<'resolver> {
    /// Creates a checked frontend over the shared async-first module source
    /// model.
    ///
    /// Call [`Self::check_async`] to await source futures. The synchronous
    /// [`Self::check`] method remains a ready-only bridge for static tools and
    /// reports pending futures as resolver diagnostics.
    #[must_use]
    pub fn new(
        module_source: &'resolver dyn ModuleSource,
        config_resolver: &'resolver dyn Resolver,
    ) -> Self {
        Self::with_checker(module_source, config_resolver, Checker::new())
    }

    /// Creates a checked frontend with caller-supplied checker state.
    ///
    /// Use this when graph checking must share a builtin environment or
    /// declaration surface with another entry point. The caller is responsible
    /// for constructing a [`Checker`] whose builtin type handles live in its own
    /// arena.
    ///
    /// Like [`Self::new`], this path supports async source futures through
    /// [`Self::check_async`]. The synchronous [`Self::check`] method remains a
    /// ready-only bridge.
    #[must_use]
    pub fn with_checker(
        module_source: &'resolver dyn ModuleSource,
        config_resolver: &'resolver dyn Resolver,
        checker: Checker,
    ) -> Self {
        let mut frontend = Frontend::new(module_source, config_resolver);
        configure_checked_frontend_parser(&mut frontend);
        Self {
            frontend,
            checker,
            checked_modules: BTreeMap::new(),
            queued_modules: BTreeSet::new(),
            environments: BTreeMap::new(),
            environment_globals: BTreeMap::new(),
            #[cfg(any())]
            prepare_module_scope: None,
            source_mode_override: None,
        }
    }

    /// Creates a checked frontend over a file resolver with caller-supplied
    /// checker state.
    ///
    /// This is internal development scaffolding for upstream fixture and
    /// expression-resolution tests. Public graph callers should pass
    /// [`ModuleSource`] through [`Self::with_checker`].
    #[doc(hidden)]
    #[must_use]
    #[cfg(any())]
    pub fn with_file_resolver_and_checker(
        file_resolver: &'resolver dyn FileResolver,
        config_resolver: &'resolver dyn Resolver,
        checker: Checker,
    ) -> Self {
        let mut frontend = Frontend::with_file_resolver(file_resolver, config_resolver);
        configure_checked_frontend_parser(&mut frontend);
        Self {
            frontend,
            checker,
            checked_modules: BTreeMap::new(),
            queued_modules: BTreeSet::new(),
            environments: BTreeMap::new(),
            environment_globals: BTreeMap::new(),
            #[cfg(any())]
            prepare_module_scope: None,
            source_mode_override: None,
        }
    }

    /// Parses and checks a root module and its statically reachable
    /// dependencies.
    pub fn check(&mut self, name: impl Into<ModuleName>) -> ParseGraphResult {
        let result = self.frontend.parse(name);
        self.finish_check(result)
    }

    /// Parses and checks a root module and its statically reachable
    /// dependencies, awaiting async [`ModuleSource`] reads and resolutions.
    pub async fn check_async(&mut self, name: impl Into<ModuleName>) -> ParseGraphResult {
        let result = self.frontend.parse_async(name).await;
        self.finish_check(result)
    }

    /// Parses and checks an implementation root and its statically reachable
    /// dependencies, then compares the root module return type with a
    /// `.d.luau`-style declaration source.
    ///
    /// This is the graph-shaped companion to [`Checker::check_conformance`]:
    /// imports are resolved through this frontend, the ordinary graph
    /// diagnostics are included, and the returned fingerprint includes the
    /// root plus dependency public interfaces.
    pub fn check_conformance(
        &mut self,
        name: impl Into<ModuleName>,
        declaration_source: &str,
    ) -> ConformanceCheck {
        let result = self.check(name);
        self.finish_conformance_check(&result, declaration_source)
    }

    /// Async source-future variant of [`Self::check_conformance`].
    pub async fn check_conformance_async(
        &mut self,
        name: impl Into<ModuleName>,
        declaration_source: &str,
    ) -> ConformanceCheck {
        let result = self.check_async(name).await;
        self.finish_conformance_check(&result, declaration_source)
    }

    fn finish_check(&mut self, result: ParseGraphResult) -> ParseGraphResult {
        for module_name in &result.build_queue {
            let Some(mut source_module) = self.frontend.source_module(module_name).cloned() else {
                continue;
            };
            if let Some(mode) = self.source_mode_override {
                source_module.mode = Some(mode);
            }
            let imported_type_bindings = self.imported_type_bindings_for(&source_module);
            let imported_require_returns = self.imported_require_returns_for(&source_module);
            let environment_type_bindings = self.environment_type_bindings_for(&source_module);
            let environment_global_bindings = self.environment_global_bindings_for(&source_module);
            #[cfg(any())]
            let prepare_module_scope = &mut self.prepare_module_scope;
            let mut checked = self
                .checker
                .check_source_module_with_require_returns_and_scope_preparer(
                    &source_module,
                    &imported_require_returns,
                    |_name, scope| {
                        install_global_bindings(scope, environment_global_bindings.iter().cloned());
                        install_type_bindings(scope, environment_type_bindings.iter().cloned());
                        install_type_bindings(
                            scope,
                            imported_type_bindings
                                .iter()
                                .cloned()
                                .map(ImportedTypeBinding::into_type_binding),
                        );
                        #[cfg(any())]
                        if let Some(prepare) = prepare_module_scope.as_deref_mut() {
                            prepare(_name, scope);
                        }
                    },
                );
            checked.extend_diagnostics(self.illegal_require_diagnostics_for(&source_module));
            self.checked_modules.insert(module_name.clone(), checked);
        }
        self.refresh_imported_modules(&result.build_queue);
        self.add_cycle_diagnostics(&result.build_queue);
        // Required exports are root obligations: the entry module must define
        // them, while imported modules are judged only by normal checking.
        // Only freshly (re)checked roots are judged — a cache-served root
        // already carries its required-export diagnostics.
        if result.build_queue.contains(&result.root)
            && let Some(checked) = self.checked_modules.get_mut(&result.root)
        {
            let required = self.checker.required_global_diagnostics(checked);
            checked.extend_diagnostics(required);
        }
        result
    }

    fn finish_conformance_check(
        &mut self,
        result: &ParseGraphResult,
        declaration_source: &str,
    ) -> ConformanceCheck {
        let fingerprint = self.conformance_fingerprint(result, declaration_source);
        let diagnostics = self.graph_diagnostics(result).into_flat_diagnostics();
        let Some(implementation) = self.checked_module(&result.root).cloned() else {
            return ConformanceCheck::new(diagnostics, fingerprint);
        };
        self.checker.conformance_report_for_checked_module(
            &implementation,
            declaration_source,
            Config::default(),
            fingerprint,
            diagnostics,
        )
    }

    /// Queues a root module for a later batch check.
    pub fn queue_module_check(&mut self, name: impl Into<ModuleName>) {
        self.queued_modules.insert(name.into());
    }

    /// Checks all queued roots and clears the queue.
    pub fn check_queued_modules(&mut self) -> Vec<ParseGraphResult> {
        let queued_modules = std::mem::take(&mut self.queued_modules);
        queued_modules
            .into_iter()
            .map(|module_name| self.check(module_name))
            .collect()
    }

    /// Checks all queued roots through the async source path and clears the
    /// queue.
    pub async fn check_queued_modules_async(&mut self) -> Vec<ParseGraphResult> {
        let queued_modules = std::mem::take(&mut self.queued_modules);
        let mut results = Vec::with_capacity(queued_modules.len());
        for module_name in queued_modules {
            results.push(self.check_async(module_name).await);
        }
        results
    }

    /// Returns a checked module result retained by this frontend.
    #[must_use]
    pub fn check_result(&self, name: &ModuleName) -> Option<&CheckedModule> {
        self.checked_module(name)
    }

    /// Installs a callback that can add module-specific bindings to the root
    /// scope before checking.
    #[cfg(any())]
    pub(crate) fn set_prepare_module_scope(
        &mut self,
        prepare: impl FnMut(&ModuleName, &mut ScopeTree) + Send + 'resolver,
    ) {
        self.prepare_module_scope = Some(Box::new(prepare));
    }

    /// Clears the module-scope preparation callback.
    #[cfg(any())]
    pub fn clear_prepare_module_scope(&mut self) {
        self.prepare_module_scope = None;
    }

    /// Sets parser options for future module refreshes.
    pub fn set_parse_options(&mut self, options: Options) {
        self.frontend.set_parse_options(options);
        self.clear_source_graph_cache();
    }

    /// Sets syntax feature flags for future module refreshes.
    pub fn set_syntax_flags(&mut self, flags: SyntaxFlags) {
        self.frontend.set_syntax_flags(flags);
        self.clear_source_graph_cache();
    }

    /// Overrides the source mode used when checking graph modules.
    ///
    /// This mirrors [`crate::checker::Config::source_mode_override`] for
    /// source graphs and can override source hot comments.
    pub fn set_source_mode_override(&mut self, mode: Option<AnalysisMode>) {
        self.source_mode_override = mode;
        self.clear_source_graph_cache();
    }

    /// Defines a type binding in a named source environment.
    pub fn define_environment_type(
        &mut self,
        environment: impl Into<String>,
        name: impl Into<String>,
        ty: Option<TypeId>,
    ) {
        let name = name.into();
        self.environments
            .entry(environment.into())
            .or_default()
            .push(TypeBinding {
                ty,
                ..TypeBinding::empty(name, TypeBindingKind::TypeAlias)
            });
    }

    /// Defines a global binding in a named source environment.
    pub fn define_environment_global(
        &mut self,
        environment: impl Into<String>,
        name: impl Into<String>,
        ty: Option<TypeId>,
    ) {
        self.environment_globals
            .entry(environment.into())
            .or_default()
            .push(EnvironmentGlobalBinding {
                name: name.into(),
                ty,
            });
    }

    /// Parses and lowers a standalone Luau type annotation.
    pub fn parse_type(&mut self, source: &str) -> Result<TypeId, Diagnostics> {
        self.checker.parse_type(source)
    }

    fn refresh_imported_modules(&mut self, module_names: &[ModuleName]) {
        for module_name in module_names {
            let imported_modules = self.imported_modules_for(module_name);
            if let Some(checked) = self.checked_modules.get_mut(module_name) {
                checked.set_imported_modules(imported_modules);
            }
        }
    }

    fn imported_modules_for(
        &self,
        name: &ModuleName,
    ) -> BTreeMap<ModuleName, ImportedModuleSummary> {
        self.frontend
            .source_node(name)
            .map(|node| {
                node.requires()
                    .iter()
                    .filter_map(|module_name| {
                        self.checked_modules
                            .get(module_name)
                            .map(|checked| (module_name.clone(), checked.import_summary()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn imported_type_bindings_for(&self, module: &SourceModule) -> Vec<ImportedTypeBinding> {
        let Some(trace) = self.frontend.require_trace(&module.name) else {
            return Vec::new();
        };
        let cyclic_modules = self.cyclic_modules_for(&module.name);
        local_require_aliases(&module.root, trace)
            .into_iter()
            .flat_map(|(local_name, module_name)| {
                let force_any = cyclic_modules.contains(&module_name);
                let any = self.checker.arena().primitives().any;
                let checked_exports =
                    self.checked_modules
                        .get(&module_name)
                        .into_iter()
                        .flat_map({
                            let local_name = local_name.clone();
                            move |checked| {
                                let exported_type_names: BTreeSet<String> =
                                    checked.exports().types().keys().cloned().collect();
                                let local_name = local_name.clone();
                                checked.exports().types().iter().map(
                                    move |(export_name, export)| {
                                        imported_type_binding_from_export(
                                            &local_name,
                                            export_name,
                                            export,
                                            checked,
                                            &exported_type_names,
                                            force_any,
                                            any,
                                        )
                                    },
                                )
                            }
                        });
                let unchecked_cycle_exports = force_any
                    .then(|| self.frontend.source_module(&module_name))
                    .flatten()
                    .into_iter()
                    .flat_map(move |source| {
                        exported_type_names(&source.root).into_iter().map({
                            let local_name = local_name.clone();
                            move |export_name| ImportedTypeBinding {
                                name: format!("{local_name}.{export_name}"),
                                display_name: Some(export_name),
                                alias_identity: None,
                                kind: TypeBindingKind::ExportedTypeAlias,
                                ty: Some(any),
                                alias: None,
                                alias_has_generics: false,
                                generic_names: Vec::new(),
                                generic_locations: Vec::new(),
                                generic_defaults: Vec::new(),
                                generic_pack_names: Vec::new(),
                                generic_pack_locations: Vec::new(),
                                generic_pack_defaults: Vec::new(),
                            }
                        })
                    });
                checked_exports.chain(unchecked_cycle_exports)
            })
            .collect()
    }

    fn environment_type_bindings_for(&self, module: &SourceModule) -> Vec<TypeBinding> {
        module
            .environment_name
            .as_ref()
            .and_then(|environment| self.environments.get(environment))
            .cloned()
            .unwrap_or_default()
    }

    fn environment_global_bindings_for(
        &self,
        module: &SourceModule,
    ) -> Vec<EnvironmentGlobalBinding> {
        module
            .environment_name
            .as_ref()
            .and_then(|environment| self.environment_globals.get(environment))
            .cloned()
            .unwrap_or_default()
    }

    fn imported_require_returns_for(
        &mut self,
        module: &SourceModule,
    ) -> BTreeMap<SyntaxId, Vec<TypeId>> {
        let Some(trace) = self.frontend.require_trace(&module.name) else {
            return BTreeMap::new();
        };
        let cyclic_modules = self.cyclic_modules_for(&module.name);
        let imports = trace
            .require_list
            .iter()
            .filter(|entry| !cyclic_modules.contains(&entry.module))
            .filter_map(|entry| {
                if let Some(checked) = self.checked_modules.get(&entry.module) {
                    Some((entry.call, checked.return_types().to_vec()))
                } else {
                    self.checker
                        .ambient_require_return(&entry.module)
                        .map(|ty| (entry.call, vec![ty]))
                }
            })
            .collect::<Vec<_>>();

        imports
            .into_iter()
            .map(|(call, returns)| {
                let returns = if returns.is_empty() {
                    vec![self.checker.arena_mut().primitives().error]
                } else {
                    returns
                        .into_iter()
                        .map(|ty| self.checker.arena_mut().publicize_type_graph(ty))
                        .collect()
                };
                (call, returns)
            })
            .collect()
    }

    fn illegal_require_diagnostics_for(&self, module: &SourceModule) -> Diagnostics {
        let Some(trace) = self.frontend.require_trace(&module.name) else {
            return Diagnostics::new();
        };
        let cyclic_modules = self.cyclic_modules_for(&module.name);
        trace
            .require_list
            .iter()
            .filter(|entry| !cyclic_modules.contains(&entry.module))
            .filter(|entry| {
                self.checked_modules
                    .get(&entry.module)
                    .is_some_and(|checked| checked.return_types().is_empty())
            })
            .map(|entry| {
                Diagnostic::error(
                    DiagnosticCategory::Resolver,
                    DiagnosticLocation::from_opt(entry.location),
                )
                .with_context("Required module does not export a value".to_owned())
            })
            .collect()
    }

    fn cyclic_modules_for(&self, module_name: &ModuleName) -> BTreeSet<ModuleName> {
        self.frontend
            .require_cycles(module_name)
            .into_iter()
            .flat_map(|cycle| cycle.path)
            .collect()
    }

    fn add_cycle_diagnostics(&mut self, module_names: &[ModuleName]) {
        for module_name in module_names {
            let Some(checked) = self.checked_modules.get(module_name) else {
                continue;
            };
            if checked.mode() == AnalysisMode::NoCheck {
                continue;
            }

            let diagnostics = self
                .frontend
                .require_cycles(module_name)
                .into_iter()
                .map(|cycle| cycle_diagnostic(module_name, &cycle))
                .collect::<Vec<_>>();
            if diagnostics.is_empty() {
                continue;
            }
            if let Some(checked) = self.checked_modules.get_mut(module_name) {
                checked.extend_diagnostics(diagnostics);
            }
        }
    }

    /// Marks a module and all known dependents dirty, dropping their stale
    /// checked results.
    pub fn mark_dirty(&mut self, name: impl Into<ModuleName>) -> Vec<ModuleName> {
        let marked = self.frontend.mark_dirty(name);
        for module_name in &marked {
            self.checked_modules.remove(module_name);
        }
        marked
    }

    /// Clears source-graph state, checked modules, and checker session state.
    pub fn clear(&mut self) {
        self.frontend.clear_cache();
        self.checker = Checker::new();
        self.checked_modules.clear();
        self.queued_modules.clear();
        self.environments.clear();
        self.environment_globals.clear();
    }

    fn clear_source_graph_cache(&mut self) {
        self.frontend.clear_cache();
        self.checked_modules.clear();
    }

    /// Returns the underlying source frontend.
    #[must_use]
    pub const fn frontend(&self) -> &Frontend<'resolver> {
        &self.frontend
    }

    /// Returns the checker session.
    #[must_use]
    pub const fn checker(&self) -> &Checker {
        &self.checker
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

    /// Iterates every diagnostic associated with a parsed graph result.
    ///
    /// The stream includes resolver diagnostics converted into
    /// [`Diagnostic`] with module display names, followed by checked-module
    /// diagnostics. Duplicate diagnostics are removed per module while
    /// preserving first occurrence order.
    pub fn graph_diagnostics(&self, result: &ParseGraphResult) -> GraphDiagnostics {
        let mut entries = Vec::new();
        for module_name in self.diagnostic_module_names(result) {
            let display_name = self.frontend.module_display_name(&module_name);
            let ambient_only = self.checker.ambient_require_return(&module_name).is_some()
                && !self.checked_modules.contains_key(&module_name);
            if !ambient_only {
                for diagnostic in self.frontend.resolver_diagnostics(&module_name) {
                    entries.push(ModuleDiagnostic {
                        module: module_name.clone(),
                        display_name: display_name.clone(),
                        diagnostic: Diagnostic::from_resolver_diagnostic_with_display_name(
                            diagnostic,
                            Some(&display_name),
                        ),
                    });
                }
            }
            if let Some(checked) = self.checked_module(&module_name) {
                for diagnostic in checked.diagnostics() {
                    entries.push(ModuleDiagnostic {
                        module: module_name.clone(),
                        display_name: display_name.clone(),
                        diagnostic: diagnostic.clone(),
                    });
                }
            }
        }
        let mut diagnostics = GraphDiagnostics::from_entries(entries);
        diagnostics.dedup();
        diagnostics
    }

    fn diagnostic_module_names(&self, result: &ParseGraphResult) -> BTreeSet<ModuleName> {
        let mut module_names = BTreeSet::new();
        module_names.insert(result.root.clone());
        module_names.extend(result.build_queue.iter().cloned());
        module_names.extend(self.checked_modules.keys().cloned());
        for (module_name, node) in self.frontend.iter_source_nodes() {
            module_names.insert(module_name.clone());
            module_names.extend(node.requires().iter().cloned());
        }
        module_names
    }

    fn conformance_fingerprint(
        &self,
        result: &ParseGraphResult,
        declaration_source: &str,
    ) -> ConformanceFingerprint {
        let mut hash = CONFORMANCE_FNV1A64_OFFSET;
        conformance_hash_update(&mut hash, b"ruau:frontend-conformance:v1\0root\0");
        conformance_hash_update(&mut hash, result.root.as_str().as_bytes());
        conformance_hash_update(&mut hash, b"\0declaration\0");
        conformance_hash_update(&mut hash, declaration_source.as_bytes());
        conformance_hash_update(&mut hash, b"\0cycle\0");
        conformance_hash_update(&mut hash, format!("{:?}", result.cycle_detected).as_bytes());
        for module_name in &result.build_queue {
            conformance_hash_update(&mut hash, b"\0module\0");
            conformance_hash_update(&mut hash, module_name.as_str().as_bytes());
            conformance_hash_update(&mut hash, b"\0display\0");
            conformance_hash_update(
                &mut hash,
                self.frontend.module_display_name(module_name).as_bytes(),
            );
            if let Some(source) = self.frontend.source_module(module_name) {
                conformance_hash_update(&mut hash, b"\0mode\0");
                conformance_hash_update(&mut hash, format!("{:?}", source.mode).as_bytes());
                conformance_hash_update(&mut hash, b"\0config\0");
                conformance_hash_update(&mut hash, format!("{:?}", source.config).as_bytes());
            }
            conformance_hash_update(&mut hash, b"\0resolver-diagnostics\0");
            conformance_hash_update(
                &mut hash,
                format!("{:?}", self.frontend.resolver_diagnostics(module_name)).as_bytes(),
            );
            if let Some(checked) = self.checked_module(module_name) {
                let snapshot = InterfaceSnapshot::from_module(self.checker.arena(), checked);
                conformance_hash_update(&mut hash, b"\0checked-diagnostics\0");
                conformance_hash_update(
                    &mut hash,
                    format!("{:?}", checked.diagnostics()).as_bytes(),
                );
                conformance_hash_update(&mut hash, b"\0interface\0");
                conformance_hash_update(&mut hash, format!("{snapshot:?}").as_bytes());
            }
        }
        ConformanceFingerprint::new(hash)
    }

    /// Returns queued root modules awaiting a batch check.
    #[must_use]
    pub const fn queued_modules(&self) -> &BTreeSet<ModuleName> {
        &self.queued_modules
    }
}

const CONFORMANCE_FNV1A64_OFFSET: u64 = 0xcbf29ce484222325;
const CONFORMANCE_FNV1A64_PRIME: u64 = 0x100000001b3;

fn conformance_hash_update(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(CONFORMANCE_FNV1A64_PRIME);
    }
}

fn install_type_bindings(scopes: &mut ScopeTree, bindings: impl IntoIterator<Item = TypeBinding>) {
    let root = scopes.root();
    for binding in bindings {
        scopes.define_type_binding(root, binding);
    }
}

fn install_global_bindings(
    scopes: &mut ScopeTree,
    bindings: impl IntoIterator<Item = EnvironmentGlobalBinding>,
) {
    let root = scopes.root();
    for binding in bindings {
        scopes.define_global(root, binding.name, binding.ty);
    }
}

fn configure_checked_frontend_parser(frontend: &mut Frontend<'_>) {
    frontend.set_parse_options(Options {
        allow_declaration_syntax: true,
        capture_comments: true,
        ..Options::default()
    });
    frontend.set_syntax_flags(SyntaxFlags::all_luau());
}

fn imported_type_binding_from_export(
    local_name: &str,
    export_name: &str,
    export: &ExportedType,
    checked: &CheckedModule,
    exported_type_names: &BTreeSet<String>,
    force_any: bool,
    any: TypeId,
) -> ImportedTypeBinding {
    let scopes = checked.scopes();
    let inline_alias = |ty| inline_private_type_aliases(ty, scopes);
    let inline_pack = |pack| inline_private_type_pack_aliases(pack, scopes);
    let generic_scope = export
        .generic_names
        .iter()
        .chain(&export.generic_pack_names)
        .cloned()
        .collect::<BTreeSet<_>>();
    let qualify_alias = |ty| {
        qualify_imported_exported_type_aliases(ty, local_name, exported_type_names, &generic_scope)
    };
    let qualify_pack = |pack| {
        qualify_imported_exported_type_pack_aliases(
            pack,
            local_name,
            exported_type_names,
            &generic_scope,
        )
    };

    let (generic_names, generic_locations, generic_defaults) = if force_any {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        (
            export.generic_names.clone(),
            export.generic_locations.clone(),
            export
                .generic_defaults
                .iter()
                .cloned()
                .map(|default| default.map(&inline_alias).map(&qualify_alias))
                .collect(),
        )
    };
    let (generic_pack_names, generic_pack_locations, generic_pack_defaults) = if force_any {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        (
            export.generic_pack_names.clone(),
            export.generic_pack_locations.clone(),
            export
                .generic_pack_defaults
                .iter()
                .cloned()
                .map(|default| default.map(&inline_pack).map(&qualify_pack))
                .collect(),
        )
    };

    ImportedTypeBinding {
        name: format!("{local_name}.{export_name}"),
        display_name: Some(export_name.to_owned()),
        alias_identity: export.alias_identity.clone(),
        kind: match export.kind {
            ExportedTypeKind::TypeAlias => TypeBindingKind::ExportedTypeAlias,
            ExportedTypeKind::Class => TypeBindingKind::Class,
            ExportedTypeKind::DeclaredClass => TypeBindingKind::DeclaredClass,
            ExportedTypeKind::TypeFunction => TypeBindingKind::TypeFunction,
        },
        ty: if force_any { Some(any) } else { export.ty },
        alias: if force_any {
            None
        } else {
            export.alias.clone().map(&inline_alias).map(&qualify_alias)
        },
        alias_has_generics: !force_any && export.alias_has_generics,
        generic_names,
        generic_locations,
        generic_defaults,
        generic_pack_names,
        generic_pack_locations,
        generic_pack_defaults,
    }
}

fn inline_private_type_aliases(mut ty: Type, scopes: &ScopeTree) -> Type {
    let mut stack = BTreeSet::new();
    inline_private_type_aliases_in_place(&mut ty, scopes, &mut stack);
    ty
}

fn inline_private_type_pack_aliases(mut pack: TypePack, scopes: &ScopeTree) -> TypePack {
    let mut stack = BTreeSet::new();
    inline_private_type_pack_aliases_in_place(&mut pack, scopes, &mut stack);
    pack
}

fn qualify_imported_exported_type_aliases(
    mut ty: Type,
    module_prefix: &str,
    exported_names: &BTreeSet<String>,
    generic_scope: &BTreeSet<String>,
) -> Type {
    qualify_imported_exported_type_aliases_in_place(
        &mut ty,
        module_prefix,
        exported_names,
        generic_scope,
    );
    ty
}

fn qualify_imported_exported_type_pack_aliases(
    mut pack: TypePack,
    module_prefix: &str,
    exported_names: &BTreeSet<String>,
    generic_scope: &BTreeSet<String>,
) -> TypePack {
    qualify_imported_exported_type_pack_aliases_in_place(
        &mut pack,
        module_prefix,
        exported_names,
        generic_scope,
    );
    pack
}

fn inline_private_type_aliases_in_place(
    ty: &mut Type,
    scopes: &ScopeTree,
    stack: &mut BTreeSet<String>,
) {
    match ty {
        Type::Reference { parameters, .. } => {
            for parameter in parameters {
                match parameter {
                    TypeParameter::Type(ty) => {
                        inline_private_type_aliases_in_place(ty, scopes, stack);
                    }
                    TypeParameter::Pack(pack) => {
                        inline_private_type_pack_aliases_in_place(pack, scopes, stack);
                    }
                }
            }
        }
        Type::Group { inner, .. } => inline_private_type_aliases_in_place(inner, scopes, stack),
        Type::Union { types, .. }
        | Type::Intersection { types, .. }
        | Type::Error { types, .. } => {
            for ty in types {
                inline_private_type_aliases_in_place(ty, scopes, stack);
            }
        }
        Type::Function {
            generics,
            generic_packs,
            arg_types,
            return_types,
            ..
        } => {
            for generic in generics {
                if let Some(default) = generic.luau_type.as_deref_mut() {
                    inline_private_type_aliases_in_place(default, scopes, stack);
                }
            }
            for generic_pack in generic_packs {
                if let Some(default) = generic_pack.luau_type.as_deref_mut() {
                    inline_private_type_pack_aliases_in_place(default, scopes, stack);
                }
            }
            for ty in &mut arg_types.types {
                inline_private_type_aliases_in_place(ty, scopes, stack);
            }
            if let Some(tail) = arg_types.tail_type.as_deref_mut() {
                inline_private_type_pack_aliases_in_place(tail, scopes, stack);
            }
            inline_private_type_pack_aliases_in_place(return_types, scopes, stack);
        }
        Type::Table { props, indexer, .. } => {
            for prop in props {
                inline_private_type_aliases_in_place(&mut prop.prop_type, scopes, stack);
            }
            if let Some(indexer) = indexer {
                inline_private_type_aliases_in_place(&mut indexer.index_type, scopes, stack);
                inline_private_type_aliases_in_place(&mut indexer.result_type, scopes, stack);
            }
        }
        Type::Typeof { .. }
        | Type::Optional { .. }
        | Type::SingletonString { .. }
        | Type::SingletonBool { .. } => {}
    }

    let replacement = match ty {
        Type::Reference {
            prefix,
            name,
            parameters,
            ..
        } if prefix.is_none() && parameters.is_empty() => {
            let name = name.as_str();
            private_inline_alias(scopes, stack, name)
        }
        _ => None,
    };

    if let Some(replacement) = replacement {
        *ty = replacement;
    }
}

fn qualify_imported_exported_type_aliases_in_place(
    ty: &mut Type,
    module_prefix: &str,
    exported_names: &BTreeSet<String>,
    generic_scope: &BTreeSet<String>,
) {
    match ty {
        Type::Reference {
            prefix,
            name,
            parameters,
            ..
        } => {
            for parameter in parameters {
                match parameter {
                    TypeParameter::Type(ty) => qualify_imported_exported_type_aliases_in_place(
                        ty,
                        module_prefix,
                        exported_names,
                        generic_scope,
                    ),
                    TypeParameter::Pack(pack) => {
                        qualify_imported_exported_type_pack_aliases_in_place(
                            pack,
                            module_prefix,
                            exported_names,
                            generic_scope,
                        );
                    }
                }
            }
            if prefix.is_none()
                && exported_names.contains(name.as_str())
                && !generic_scope.contains(name.as_str())
            {
                *prefix = Some(Name::new(module_prefix));
            }
        }
        Type::Typeof { .. } | Type::Optional { .. } => {}
        Type::Group { inner, .. } => {
            qualify_imported_exported_type_aliases_in_place(
                inner,
                module_prefix,
                exported_names,
                generic_scope,
            );
        }
        Type::Union { types, .. } | Type::Intersection { types, .. } => {
            for ty in types {
                qualify_imported_exported_type_aliases_in_place(
                    ty,
                    module_prefix,
                    exported_names,
                    generic_scope,
                );
            }
        }
        Type::Function {
            generics,
            generic_packs,
            arg_types,
            return_types,
            ..
        } => {
            let mut nested_generics = generic_scope.clone();
            nested_generics.extend(
                generics
                    .iter()
                    .map(|generic| generic.name.as_str().to_owned()),
            );
            nested_generics.extend(
                generic_packs
                    .iter()
                    .map(|generic| generic.name.as_str().to_owned()),
            );
            for generic in generics {
                if let Some(default) = &mut generic.luau_type {
                    qualify_imported_exported_type_aliases_in_place(
                        default,
                        module_prefix,
                        exported_names,
                        &nested_generics,
                    );
                }
            }
            for generic in generic_packs {
                if let Some(default) = &mut generic.luau_type {
                    qualify_imported_exported_type_pack_aliases_in_place(
                        default,
                        module_prefix,
                        exported_names,
                        &nested_generics,
                    );
                }
            }
            for ty in &mut arg_types.types {
                qualify_imported_exported_type_aliases_in_place(
                    ty,
                    module_prefix,
                    exported_names,
                    &nested_generics,
                );
            }
            if let Some(tail) = &mut arg_types.tail_type {
                qualify_imported_exported_type_pack_aliases_in_place(
                    tail,
                    module_prefix,
                    exported_names,
                    &nested_generics,
                );
            }
            qualify_imported_exported_type_pack_aliases_in_place(
                return_types,
                module_prefix,
                exported_names,
                &nested_generics,
            );
        }
        Type::Table { props, indexer, .. } => {
            for prop in props {
                qualify_imported_exported_type_aliases_in_place(
                    &mut prop.prop_type,
                    module_prefix,
                    exported_names,
                    generic_scope,
                );
            }
            if let Some(indexer) = indexer {
                qualify_imported_exported_type_aliases_in_place(
                    &mut indexer.index_type,
                    module_prefix,
                    exported_names,
                    generic_scope,
                );
                qualify_imported_exported_type_aliases_in_place(
                    &mut indexer.result_type,
                    module_prefix,
                    exported_names,
                    generic_scope,
                );
            }
        }
        Type::SingletonString { .. } | Type::SingletonBool { .. } | Type::Error { .. } => {}
    }
}

fn qualify_imported_exported_type_pack_aliases_in_place(
    pack: &mut TypePack,
    module_prefix: &str,
    exported_names: &BTreeSet<String>,
    generic_scope: &BTreeSet<String>,
) {
    match pack {
        TypePack::Explicit { type_list, .. } => {
            for ty in &mut type_list.types {
                qualify_imported_exported_type_aliases_in_place(
                    ty,
                    module_prefix,
                    exported_names,
                    generic_scope,
                );
            }
            if let Some(tail) = &mut type_list.tail_type {
                qualify_imported_exported_type_pack_aliases_in_place(
                    tail,
                    module_prefix,
                    exported_names,
                    generic_scope,
                );
            }
        }
        TypePack::Variadic { variadic_type, .. } => {
            qualify_imported_exported_type_aliases_in_place(
                variadic_type,
                module_prefix,
                exported_names,
                generic_scope,
            );
        }
        TypePack::Generic { .. } => {}
    }
}

fn inline_private_type_pack_aliases_in_place(
    pack: &mut TypePack,
    scopes: &ScopeTree,
    stack: &mut BTreeSet<String>,
) {
    match pack {
        TypePack::Explicit { type_list, .. } => {
            for ty in &mut type_list.types {
                inline_private_type_aliases_in_place(ty, scopes, stack);
            }
            if let Some(tail) = type_list.tail_type.as_deref_mut() {
                inline_private_type_pack_aliases_in_place(tail, scopes, stack);
            }
        }
        TypePack::Variadic { variadic_type, .. } => {
            inline_private_type_aliases_in_place(variadic_type, scopes, stack);
        }
        TypePack::Generic { .. } => {}
    }
}

fn private_inline_alias(
    scopes: &ScopeTree,
    stack: &mut BTreeSet<String>,
    name: &str,
) -> Option<Type> {
    if !stack.insert(name.to_owned()) {
        return None;
    }
    let binding = scopes.get(scopes.root()).type_bindings.get(name);
    let mut replacement = binding.and_then(|binding| {
        (!binding.exported && !binding.alias_has_generics)
            .then_some(binding.alias.clone())
            .flatten()
    });
    if let Some(replacement) = &mut replacement {
        inline_private_type_aliases_in_place(replacement, scopes, stack);
    }
    stack.remove(name);
    replacement
}

impl ImportedTypeBinding {
    fn into_type_binding(self) -> TypeBinding {
        let mut b = TypeBinding::empty(self.name, self.kind);
        b.display_name = self.display_name;
        b.alias_identity = self.alias_identity;
        b.ty = self.ty;
        b.alias = self.alias;
        b.alias_has_generics = self.alias_has_generics;
        b.generic_names = self.generic_names;
        b.generic_locations = self.generic_locations;
        b.generic_defaults = self.generic_defaults;
        b.generic_pack_names = self.generic_pack_names;
        b.generic_pack_locations = self.generic_pack_locations;
        b.generic_pack_defaults = self.generic_pack_defaults;
        b
    }
}

fn cycle_diagnostic(module_name: &ModuleName, cycle: &RequireCycle) -> Diagnostic {
    let cycle_modules = cycle
        .path
        .iter()
        .map(|module| module.as_str().to_owned())
        .collect::<Vec<_>>();
    let context = format!("Cyclic module dependency: {}", cycle_modules.join(" -> "));
    Diagnostic::error(
        DiagnosticCategory::Resolver,
        DiagnosticLocation::from_opt(cycle.location),
    )
    .with_context(context)
    .with_typed(crate::diagnostics::Payload::RequireCycle {
        module: module_name.as_str().to_owned(),
        cycle: cycle_modules,
    })
}

fn local_require_aliases(
    root: &Stat,
    trace: &ruau_analysis::RequireTraceResult,
) -> Vec<(String, ModuleName)> {
    let mut aliases = Vec::new();
    if let Stat::Block { body, .. } = root {
        for stat in body {
            collect_local_require_aliases(stat, trace, &mut aliases);
        }
    }
    aliases
}

fn collect_local_require_aliases(
    stat: &Stat,
    trace: &ruau_analysis::RequireTraceResult,
    aliases: &mut Vec<(String, ModuleName)>,
) {
    if let Stat::Local { vars, values, .. } = stat {
        for (local, value) in vars.iter().zip(values) {
            if let Some(module) = required_module_for_expr(value, trace) {
                aliases.push((local.name.as_str().to_owned(), module.clone()));
            }
        }
    }
}

fn exported_type_names(root: &Stat) -> Vec<String> {
    match root {
        Stat::Block { body, .. } => body.iter().flat_map(exported_type_names).collect(),
        Stat::TypeAlias { name, exported, .. } if *exported => vec![name.as_str().to_owned()],
        _ => Vec::new(),
    }
}

fn required_module_for_expr<'trace>(
    expr: &Expr,
    trace: &'trace ruau_analysis::RequireTraceResult,
) -> Option<&'trace ModuleName> {
    trace
        .module_for(expr.syntax_id())
        .map(|module| &module.name)
        .or_else(|| match expr {
            Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
                required_module_for_expr(expr, trace)
            }
            _ => None,
        })
}

#[cfg(any())]
mod tests;
