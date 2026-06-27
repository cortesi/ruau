use std::{collections::BTreeMap, sync::Arc};

use ruau_analysis::{
    SourceModule,
    resolve::{AnalysisMode, config::AnalysisConfig},
};
use ruau_ast::{
    parse::{ParseResult, parse_file_bytes_with, parse_file_with},
    syntax::{Expr, LocalId, Stat, SyntaxId},
    visit::{NodePath, Visitor, WalkControl, walk_stat},
};
use ruau_source::ModuleName;

use super::{
    CheckedModule, Checker, Config, empty_root,
    module_surface::{
        collect_exports, collect_module_return_types, type_definition_issue_diagnostics,
    },
};
use crate::{
    constraints::{Constraint, ConstraintSolveError, ConstraintSolveSummary, ConstraintSolver},
    dfg::DataFlowGraph,
    diagnostic::{DiagnosticCategory, Severity, TypeDiagnostic},
    diagnostic_selection::select_constraint_errors_for_reporting,
    generation::{
        GenerationConfig, generate_expression_constraints_with_require_returns,
        operator::{
            DeferredBinaryOperatorDiagnostic, DeferredUnaryOperatorDiagnostic,
            deferred_binary_operator_diagnostic, deferred_unary_operator_diagnostic,
        },
    },
    generic_alias,
    post_solve::{check_solved_expressions, check_strict_statements},
    queries::Queries,
    scopes::{ScopeId, ScopeTree, ValueBindingKind},
    types::TypeId,
};

struct AmbientRequireReturnCollector<'a> {
    ambient: &'a BTreeMap<ModuleName, TypeId>,
    returns: &'a mut BTreeMap<SyntaxId, Vec<TypeId>>,
}

impl Visitor<'_> for AmbientRequireReturnCollector<'_> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        let Expr::Call {
            syntax_id,
            func,
            args,
            ..
        } = expr
        else {
            return WalkControl::Continue;
        };
        if !matches!(func.as_ref(), Expr::Global { name, .. } if name.as_str() == "require") {
            return WalkControl::Continue;
        }
        let Some(Expr::String { value, .. }) = args.first() else {
            return WalkControl::Continue;
        };
        let module = ModuleName::from(value.as_str());
        if let Some(ty) = self.ambient.get(&module) {
            self.returns.entry(*syntax_id).or_insert_with(|| vec![*ty]);
        }
        WalkControl::Continue
    }
}

impl Checker {
    /// Checks source text with default checker configuration.
    pub fn check_source(&mut self, source: &str) -> CheckedModule {
        self.check_source_with_config(source, Config::default())
    }

    /// Checks source text with explicit checker configuration.
    pub fn check_source_with_config(&mut self, source: &str, config: Config) -> CheckedModule {
        let parsed = parse_file_with(source, config.parse_options, config.syntax_flags);
        self.check_parse_result_with_config_and_required(parsed, config, true)
    }

    /// Checks arbitrary source bytes with explicit checker configuration.
    pub fn check_source_bytes_with_config(
        &mut self,
        source: &[u8],
        config: Config,
    ) -> CheckedModule {
        let parsed = parse_file_bytes_with(source, config.parse_options, config.syntax_flags);
        self.check_parse_result_with_config_and_required(parsed, config, true)
    }

    pub(crate) fn check_source_with_config_without_required(
        &mut self,
        source: &str,
        config: Config,
    ) -> CheckedModule {
        let parsed = parse_file_with(source, config.parse_options, config.syntax_flags);
        self.check_parse_result_with_config_and_required(parsed, config, false)
    }

    fn check_parse_result_with_config_and_required(
        &mut self,
        parsed: ParseResult,
        config: Config,
        judge_required_globals: bool,
    ) -> CheckedModule {
        let mode = config.source_mode_override.unwrap_or_else(|| {
            ruau_analysis::resolve::effective_mode(
                &parsed.errors,
                &parsed.hot_comments,
                config.analysis.mode(),
            )
            .unwrap_or(config.default_mode)
        });
        let diagnostics = parsed
            .errors
            .iter()
            .map(TypeDiagnostic::from)
            .collect::<Vec<_>>();
        let root = Arc::new(parsed.root.unwrap_or_else(empty_root));

        self.check_parsed_with_parts_and_required(
            root,
            mode,
            config.analysis,
            config.generation,
            diagnostics,
            judge_required_globals,
        )
    }

    /// Checks a parsed module root with default checker configuration.
    pub fn check_parsed(&mut self, root: &Stat) -> CheckedModule {
        self.check_parsed_with_config(root, Config::default())
    }

    /// Checks a parsed module root with explicit checker configuration.
    pub fn check_parsed_with_config(&mut self, root: &Stat, config: Config) -> CheckedModule {
        self.check_parsed_with_parts(
            Arc::new(root.clone()),
            config.analysis.mode().unwrap_or(config.default_mode),
            config.analysis,
            config.generation,
            Vec::new(),
        )
    }

    /// Checks a parsed module root with explicit checker configuration.
    pub fn check_stat_with_config(&mut self, root: &Stat, config: Config) -> CheckedModule {
        self.check_parsed_with_config(root, config)
    }

    /// Checks a parsed source module with known return surfaces for static
    /// `require` calls.
    pub(crate) fn check_source_module_with_require_returns_and_scope_preparer(
        &mut self,
        module: &SourceModule,
        require_return_types: &BTreeMap<SyntaxId, Vec<TypeId>>,
        prepare_scope: impl FnOnce(&ModuleName, &mut ScopeTree),
    ) -> CheckedModule {
        let mode = if module.parse_errors.is_empty() {
            module.mode.unwrap_or(AnalysisMode::Strict)
        } else {
            AnalysisMode::NoCheck
        };
        let diagnostics = module
            .parse_errors
            .iter()
            .map(TypeDiagnostic::from)
            .collect::<Vec<_>>();
        self.check_parsed_with_parts_prepared(
            // One unavoidable deep clone: `SourceModule` (ruau-analysis) owns
            // its root as a bare `Stat` public field. Sharing it too needs a
            // broader AST artifact ownership pass.
            Arc::new(module.root.clone()),
            mode,
            module.config.clone(),
            GenerationConfig::default(),
            module.name.as_str().to_owned(),
            diagnostics,
            require_return_types,
            |scopes| prepare_scope(&module.name, scopes),
        )
    }

    fn check_parsed_with_parts(
        &mut self,
        root: Arc<Stat>,
        mode: AnalysisMode,
        config: AnalysisConfig,
        generation_config: GenerationConfig,
        diagnostics: Vec<TypeDiagnostic>,
    ) -> CheckedModule {
        self.check_parsed_with_parts_and_required(
            root,
            mode,
            config,
            generation_config,
            diagnostics,
            true,
        )
    }

    fn check_parsed_with_parts_and_required(
        &mut self,
        root: Arc<Stat>,
        mode: AnalysisMode,
        config: AnalysisConfig,
        generation_config: GenerationConfig,
        diagnostics: Vec<TypeDiagnostic>,
        judge_required_globals: bool,
    ) -> CheckedModule {
        let alias_module = self.next_standalone_alias_module();
        let mut checked = self.check_parsed_with_parts_prepared(
            root,
            mode,
            config,
            generation_config,
            alias_module,
            diagnostics,
            &BTreeMap::new(),
            |_| {},
        );
        if judge_required_globals {
            let required = self.required_global_diagnostics(&checked);
            checked.extend_diagnostics(required);
        }
        checked
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn check_parsed_with_parts_prepared(
        &mut self,
        root: Arc<Stat>,
        mode: AnalysisMode,
        config: AnalysisConfig,
        generation_config: GenerationConfig,
        alias_module: String,
        mut diagnostics: Vec<TypeDiagnostic>,
        require_return_types: &BTreeMap<SyntaxId, Vec<TypeId>>,
        prepare_scope: impl FnOnce(&mut ScopeTree),
    ) -> CheckedModule {
        let (mut scopes, dfg) =
            self.prepare_module_scope(&root, &config, alias_module, prepare_scope);
        let require_return_types = self.require_return_types_for_root(&root, require_return_types);
        let mut query_local_types = if mode == AnalysisMode::NoCheck {
            crate::query_surface::recover_nocheck_query_local_types(
                &root,
                &scopes,
                &dfg,
                &mut self.arena,
            )
        } else {
            BTreeMap::new()
        };
        let stage = if mode == AnalysisMode::NoCheck {
            SolveStage::default()
        } else {
            self.generate_and_solve(
                &root,
                &scopes,
                &dfg,
                mode,
                generation_config,
                &require_return_types,
                &mut diagnostics,
                &mut query_local_types,
            )
        };
        let SolvedConstraints {
            constraints,
            queries,
            solve_summary,
            mut global_defs,
        } = self.render_solve_diagnostics(mode, stage, &mut diagnostics);
        let exports = collect_exports(&scopes, &dfg, &mut self.arena, mode);
        let return_types = collect_module_return_types(&root, mode, &queries, &mut self.arena);
        self.render_module_diagnostics(
            &root,
            mode,
            &config,
            &queries,
            &mut scopes,
            &dfg,
            &global_defs,
            &mut diagnostics,
        );
        crate::query_surface::generalize_query_types_post_solve(
            &root,
            &dfg,
            &mut self.arena,
            mode,
            &queries,
            &mut global_defs,
            &mut query_local_types,
        );

        CheckedModule {
            root,
            mode,
            config,
            diagnostics,
            scopes,
            dfg,
            queries,
            exports,
            return_types,
            imported_modules: BTreeMap::new(),
            constraints,
            solve_summary,
            global_defs,
            query_local_types,
        }
    }

    /// Module scope and DFG setup: builds the scope tree with builtin and
    /// config globals installed, applies the caller's scope preparation,
    /// populates module bindings (plus recovered const-`nil` reference
    /// bindings), and builds the data-flow graph.
    fn prepare_module_scope(
        &mut self,
        root: &Stat,
        config: &AnalysisConfig,
        alias_module: String,
        prepare_scope: impl FnOnce(&mut ScopeTree),
    ) -> (ScopeTree, DataFlowGraph) {
        let mut scopes = ScopeTree::new_with_alias_module(Some(alias_module));
        let root_scope = scopes.root();
        self.builtins.install_into_scope(&mut scopes, root_scope);
        install_config_globals(config, &mut scopes, root_scope, self.arena.primitives().any);
        prepare_scope(&mut scopes);
        scopes.populate_module_bindings(root);
        define_recovered_const_nil_ref_bindings(root, &mut scopes, self.arena.primitives().nil);
        let dfg = DataFlowGraph::build(root, &scopes, &mut self.arena);
        (scopes, dfg)
    }

    fn require_return_types_for_root(
        &self,
        root: &Stat,
        require_return_types: &BTreeMap<SyntaxId, Vec<TypeId>>,
    ) -> BTreeMap<SyntaxId, Vec<TypeId>> {
        let mut merged = require_return_types.clone();
        if self.ambient_require_returns.is_empty() {
            return merged;
        }
        let mut collector = AmbientRequireReturnCollector {
            ambient: &self.ambient_require_returns,
            returns: &mut merged,
        };
        walk_stat(root, &mut collector);
        merged
    }

    /// Constraint generation and solving: generates expression constraints
    /// for the module, runs the solver, and returns the raw solve output —
    /// solver errors and deferred diagnostics still unselected and
    /// unrendered. Generation-time diagnostics and query-only local types
    /// are recorded directly.
    #[allow(clippy::too_many_arguments)]
    fn generate_and_solve(
        &mut self,
        root: &Stat,
        scopes: &ScopeTree,
        dfg: &DataFlowGraph,
        mode: AnalysisMode,
        generation_config: GenerationConfig,
        require_return_types: &BTreeMap<SyntaxId, Vec<TypeId>>,
        diagnostics: &mut Vec<TypeDiagnostic>,
        query_local_types: &mut BTreeMap<LocalId, TypeId>,
    ) -> SolveStage {
        let generated = generate_expression_constraints_with_require_returns(
            root,
            scopes,
            dfg,
            &mut self.arena,
            mode,
            generation_config,
            require_return_types,
        );
        query_local_types.extend(generated.query_local_types);
        diagnostics.extend(generated.diagnostics);
        let mut solver = ConstraintSolver::new(&mut self.arena);
        if let Some(cancel) = &self.cancel {
            solver.set_cancel_flag(std::sync::Arc::clone(cancel));
        }
        for constraint in &generated.constraints {
            solver.push(constraint.clone());
        }
        let (summary, errors) = solver.solve_collecting();
        self.arena.finalize_unsealed_tables();
        SolveStage {
            constraints: generated.constraints,
            queries: generated.queries,
            global_defs: generated.global_defs,
            attempt: Some(SolveAttempt { summary, errors }),
            deferred_diagnostics: generated.deferred_diagnostics,
            deferred_binary_operator_diagnostics: generated.deferred_binary_operator_diagnostics,
            deferred_unary_operator_diagnostics: generated.deferred_unary_operator_diagnostics,
        }
    }

    /// Solve-diagnostic selection and rendering: filters suppressed solver
    /// errors, selects the errors worth reporting, renders them (with
    /// overload and generic-count companions) plus the deferred operator
    /// diagnostics, and returns the constraint state the checked module
    /// retains.
    fn render_solve_diagnostics(
        &self,
        mode: AnalysisMode,
        stage: SolveStage,
        diagnostics: &mut Vec<TypeDiagnostic>,
    ) -> SolvedConstraints {
        let SolveStage {
            constraints,
            queries,
            global_defs,
            attempt,
            deferred_diagnostics,
            deferred_binary_operator_diagnostics,
            deferred_unary_operator_diagnostics,
        } = stage;
        let solve_summary = attempt.and_then(|SolveAttempt { summary, errors }| {
            let suppress_nilable_reads = mode == AnalysisMode::Nonstrict;
            let errors = errors
                .into_iter()
                .filter(|error| !error.is_fully_suppressing())
                .filter(|error| !(suppress_nilable_reads && error.is_nilable_property_read()))
                .collect::<Vec<_>>();
            let errors = select_constraint_errors_for_reporting(errors);
            if errors.is_empty() {
                Some(summary)
            } else {
                diagnostics.extend(errors.into_iter().flat_map(|error| {
                    let main = error.into_diagnostic_with_arena(Some(&self.arena));
                    let overload_companion = overload_available_overloads_companion(&main);
                    let generic_count_companion = generic_count_mismatch_companion(&main);
                    std::iter::once(main)
                        .chain(overload_companion)
                        .chain(generic_count_companion)
                }));
                None
            }
        });
        diagnostics.extend(deferred_binary_operator_diagnostics.into_iter().filter_map(
            |diagnostic| {
                deferred_binary_operator_diagnostic(
                    &self.arena,
                    &constraints,
                    &global_defs,
                    &diagnostic,
                )
            },
        ));
        diagnostics.extend(
            deferred_unary_operator_diagnostics
                .into_iter()
                .filter_map(|diagnostic| {
                    deferred_unary_operator_diagnostic(&self.arena, &diagnostic)
                }),
        );
        diagnostics.extend(deferred_diagnostics);
        SolvedConstraints {
            constraints,
            queries,
            solve_summary,
            global_defs,
        }
    }

    /// Module-level diagnostic passes and finalization: strict-statement and
    /// solved-expression checks, root type-alias validation and
    /// materialization, then dedup, standalone-incomplete suppression, and
    /// config severity mapping.
    #[allow(clippy::too_many_arguments)]
    fn render_module_diagnostics(
        &mut self,
        root: &Stat,
        mode: AnalysisMode,
        config: &AnalysisConfig,
        queries: &Queries,
        scopes: &mut ScopeTree,
        dfg: &DataFlowGraph,
        global_defs: &BTreeMap<String, TypeId>,
        diagnostics: &mut Vec<TypeDiagnostic>,
    ) {
        diagnostics.extend(check_strict_statements(root, mode));
        diagnostics.extend(check_solved_expressions(root, mode, queries, &self.arena));
        if mode != AnalysisMode::NoCheck {
            diagnostics.extend(generic_alias::validate_root_type_aliases(
                scopes,
                global_defs,
            ));
            diagnostics.extend(type_definition_issue_diagnostics(root));
        }
        diagnostics.extend(generic_alias::materialize_root_type_aliases(
            scopes,
            dfg,
            &mut self.arena,
            mode,
            global_defs,
        ));
        crate::diagnostic::dedup(diagnostics);
        suppress_standalone_constraint_solving_incomplete(diagnostics);
        apply_config_diagnostic_severity(config, diagnostics);
    }

    fn next_standalone_alias_module(&mut self) -> String {
        let id = self.next_standalone_alias_module;
        self.next_standalone_alias_module += 1;
        format!("<source:{id}>")
    }
}

/// Constraint generation and solve output for one module, before diagnostic
/// selection and rendering. `NoCheck` modules use the default (empty) stage:
/// no constraints, no solve attempt, nothing deferred.
#[derive(Default)]
struct SolveStage {
    constraints: Vec<Constraint>,
    queries: Queries,
    global_defs: BTreeMap<String, TypeId>,
    attempt: Option<SolveAttempt>,
    deferred_diagnostics: Vec<TypeDiagnostic>,
    deferred_binary_operator_diagnostics: Vec<DeferredBinaryOperatorDiagnostic>,
    deferred_unary_operator_diagnostics: Vec<DeferredUnaryOperatorDiagnostic>,
}

/// One completed solver pass: its summary plus the raw, unfiltered errors.
struct SolveAttempt {
    summary: ConstraintSolveSummary,
    errors: Vec<ConstraintSolveError>,
}

/// Constraint state retained on the [`CheckedModule`] after diagnostic
/// rendering consumed the rest of a [`SolveStage`].
struct SolvedConstraints {
    constraints: Vec<Constraint>,
    queries: Queries,
    solve_summary: Option<ConstraintSolveSummary>,
    global_defs: BTreeMap<String, TypeId>,
}

fn define_recovered_const_nil_ref_bindings(root: &Stat, scopes: &mut ScopeTree, nil: TypeId) {
    // The parser keeps upstream's `AstStatError` for a const missing an
    // initializer, but later references still resolve to the recovered const.
    // Recreate that orphan binding for by-name queries without adding a DFG def.
    struct ConstRefCollector {
        refs: Vec<ruau_ast::syntax::LocalRef>,
    }

    impl<'ast> Visitor<'ast> for ConstRefCollector {
        fn visit_expr(&mut self, _path: &NodePath, expr: &'ast Expr) -> WalkControl {
            if let Expr::Local { local, .. } = expr
                && local.is_const
            {
                self.refs.push(local.clone());
            }
            WalkControl::Continue
        }
    }

    let mut collector = ConstRefCollector { refs: Vec::new() };
    walk_stat(root, &mut collector);
    for local in collector.refs {
        if scopes.lookup_local_id(local.id).is_none() {
            scopes.define_local_with_kind(
                scopes.root(),
                local.id,
                local.name.as_str(),
                ValueBindingKind::Local,
                Some(nil),
            );
        }
    }
}

fn install_config_globals(
    config: &AnalysisConfig,
    scopes: &mut ScopeTree,
    scope: ScopeId,
    any: TypeId,
) {
    for global in config.globals() {
        scopes.define_global(scope, global, Some(any));
    }
}

fn apply_config_diagnostic_severity(config: &AnalysisConfig, diagnostics: &mut [TypeDiagnostic]) {
    if config.type_errors() {
        return;
    }

    for diagnostic in diagnostics {
        if matches!(
            diagnostic.category,
            DiagnosticCategory::Parse | DiagnosticCategory::Resolver
        ) {
            continue;
        }
        diagnostic.severity = Severity::Warning;
    }
}

/// Builds the upstream "Available overloads" companion for a no-overload-match
/// call diagnostic. Upstream pairs the call error with an `ExtraInformation`
/// follow-up listing the candidate signatures, but only for genuine overload
/// sets (two or more candidates); a single non-matching function signature
/// reports just the call error. The candidate summaries are already attached
/// to the primary diagnostic's payload during overload-error lowering.
fn overload_available_overloads_companion(diagnostic: &TypeDiagnostic) -> Option<TypeDiagnostic> {
    if diagnostic.category != DiagnosticCategory::Call {
        return None;
    }
    let crate::diagnostic::Payload::NoOverloadMatch {
        available_overloads,
        ..
    } = &diagnostic.typed_payload
    else {
        return None;
    };
    if available_overloads.len() < 2 {
        return None;
    }
    let message = format!(
        "Available overloads: {}",
        join_overload_summaries(available_overloads)
    );
    Some(
        TypeDiagnostic::error(DiagnosticCategory::Call, diagnostic.primary_location)
            .with_context(message)
            .with_typed(crate::diagnostic::Payload::OverloadCandidates {
                candidates: available_overloads.clone(),
            }),
    )
}

/// Builds the upstream `GenericTypeCountMismatch` companion for a function
/// subtype failure whose candidate has fewer generic parameters than the
/// required type. The structured counts were attached to the primary
/// diagnostic during subtype-error lowering; the companion is emitted after
/// error aggregation so it is not collapsed into the primary mismatch.
fn generic_count_mismatch_companion(diagnostic: &TypeDiagnostic) -> Option<TypeDiagnostic> {
    let mismatch = diagnostic.typed_payload.generic_count_mismatch()?;
    let subtype_count = mismatch.subtype_count;
    let supertype_count = mismatch.supertype_count;
    let message = match mismatch.parameter {
        crate::diagnostic::GenericParameterKind::Type => format!(
            "Different number of generic type parameters: subtype had {subtype_count}, \
             supertype had {supertype_count}."
        ),
        crate::diagnostic::GenericParameterKind::Pack => format!(
            "Different number of generic type pack parameters: subtype had {subtype_count}, \
             supertype had {supertype_count}."
        ),
    };
    Some(
        TypeDiagnostic::error(DiagnosticCategory::Generic, diagnostic.primary_location)
            .with_context(message)
            .with_typed(crate::diagnostic::Payload::GenericCountMismatch(
                mismatch.clone(),
            )),
    )
}

/// Joins overload candidate summaries the way upstream renders them:
/// `A; B; and C`, with a trailing `and` before the final candidate.
fn join_overload_summaries(candidates: &[String]) -> String {
    match candidates {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{}; and {last}", rest.join("; ")),
    }
}

fn suppress_standalone_constraint_solving_incomplete(diagnostics: &mut Vec<TypeDiagnostic>) {
    if diagnostics.len() != 1 {
        return;
    }
    let diagnostic = &diagnostics[0];
    if diagnostic.category == DiagnosticCategory::Constraint
        && matches!(
            diagnostic.typed_payload,
            crate::diagnostic::Payload::ConstraintSolvingIncompleteForced
        )
    {
        diagnostics.clear();
    }
}
