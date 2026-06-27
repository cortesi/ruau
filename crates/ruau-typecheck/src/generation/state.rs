//! Expression constraint generation for single-module checking.

use std::collections::{BTreeMap, BTreeSet};

use ruau_analysis::resolve::AnalysisMode;
use ruau_ast::{
    Location,
    json::JsonTableItemKind,
    syntax::{Expr, Local, LocalId, SyntaxId, TableItem},
};

use crate::{
    checker::GenerationConfig,
    constraints::Constraint,
    dfg::{DataFlowGraph, RefinementKey, RefinementMap},
    diagnostic::{DiagnosticCategory, DiagnosticLocation, TypeDiagnostic},
    generation::operator::{DeferredBinaryOperatorDiagnostic, DeferredUnaryOperatorDiagnostic},
    normalize::simplify_type,
    queries::Queries,
    scopes::{ScopeId, ScopeTree, Symbol, ValueBindingKind},
    types::{
        Arena, PrimitiveType, PrimitiveTypes, SingletonType, TableAliasIdentity, TableIndexer,
        TableProperty, TableState, TableType, TypeId, TypeKind, TypePackId, TypePackKind,
    },
};

/// Generated constraints plus query data.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GeneratedConstraints {
    /// Constraints emitted from expressions and statements.
    pub constraints: Vec<Constraint>,
    /// Query data collected during generation.
    pub queries: Queries,
    /// Recoverable generation diagnostics.
    pub diagnostics: Vec<TypeDiagnostic>,
    /// Constraint-like diagnostics that should be reported after generation
    /// diagnostics, preserving the old solver-error ordering while still
    /// allowing multiple eager local annotation errors.
    pub deferred_diagnostics: Vec<TypeDiagnostic>,
    /// Binary-operator diagnostics for global function parameters that need
    /// solved argument types before the checker knows whether the operation is
    /// truly invalid.
    pub deferred_binary_operator_diagnostics: Vec<DeferredBinaryOperatorDiagnostic>,
    /// Unary-operator diagnostics for unannotated function parameters that need
    /// solved bounds before the checker knows whether the operation is invalid.
    pub deferred_unary_operator_diagnostics: Vec<DeferredUnaryOperatorDiagnostic>,
    /// Inferred types of top-level global function/value definitions, keyed by
    /// name. Surfaced so post-solve passes (root type-alias materialization)
    /// can resolve `typeof(globalFn())` the same way locals already resolve
    /// through the scope tree.
    pub global_defs: BTreeMap<String, TypeId>,
    /// Local type answers that differ from value-flow storage. A by-name query
    /// (`requireType("s")`) reports these handles, while value-flow and
    /// assignment checks keep using the DFG/local binding. Query-only: never
    /// consulted during checking.
    pub query_local_types: BTreeMap<LocalId, TypeId>,
}

#[derive(Clone, Copy)]
pub struct IndexExprLocations {
    pub(crate) expr: Option<Location>,
    pub(crate) index: Option<DiagnosticLocation>,
}

pub struct IndexNameBinding<'b> {
    pub(crate) location: Option<Location>,
    pub(crate) syntax_id: SyntaxId,
    pub(crate) expr_ty: TypeId,
    pub(crate) base_ty: TypeId,
    pub(crate) index: &'b str,
    pub(crate) grow_free_parameter_table: bool,
    pub(crate) grow_refinement_probe_table: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedNilQueryRead {
    pub(crate) syntax_id: SyntaxId,
    pub(crate) location: Option<Location>,
    pub(crate) path: Vec<String>,
}

pub struct GenerationInput<'a> {
    pub(crate) scopes: &'a ScopeTree,
    pub(crate) dfg: &'a DataFlowGraph,
    pub(crate) mode: AnalysisMode,
    pub(crate) config: GenerationConfig,
    pub(crate) require_return_types: &'a BTreeMap<SyntaxId, Vec<TypeId>>,
}

#[derive(Default)]
pub struct AliasLoweringState {
    pub(crate) class_lowering_placeholders: BTreeMap<(ScopeId, String), TypeId>,
    pub(crate) type_alias_stack: Vec<String>,
    pub(crate) type_alias_definition_stack: Vec<TableAliasIdentity>,
    pub(crate) type_alias_cache: BTreeMap<TableAliasIdentity, TypeId>,
    pub(crate) generic_type_alias_cache:
        BTreeMap<(TableAliasIdentity, Vec<TypeId>, Vec<TypePackId>), TypeId>,
    pub(crate) type_alias_function_depth: usize,
    pub(crate) generic_alias_type_argument_depth: usize,
    pub(crate) generic_type_cache: BTreeMap<(ScopeId, String), TypeId>,
    pub(crate) generic_type_pack_cache: BTreeMap<(ScopeId, String), TypePackId>,
    pub(crate) generic_type_substitutions: Vec<BTreeMap<String, TypeId>>,
    pub(crate) generic_type_pack_substitutions: Vec<BTreeMap<String, TypePackId>>,
}

const TYPE_FUNCTION_EVALUATION_DEPTH_LIMIT: usize = 128;
const TYPE_FUNCTION_EVALUATION_FUEL_LIMIT: usize = 16_384;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TypeFunctionEvaluationFrame {
    scope: ScopeId,
    name: String,
    arguments: Vec<TypeId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypeFunctionEvaluationLimit {
    RecursiveCall,
    DepthExceeded,
    FuelExhausted,
}

impl TypeFunctionEvaluationLimit {
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::RecursiveCall => "recursive-call",
            Self::DepthExceeded => "depth-limit",
            Self::FuelExhausted => "evaluation-fuel",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeFunctionEvaluationState {
    stack: Vec<TypeFunctionEvaluationFrame>,
    remaining_fuel: usize,
}

impl Default for TypeFunctionEvaluationState {
    fn default() -> Self {
        Self {
            stack: Vec::new(),
            remaining_fuel: TYPE_FUNCTION_EVALUATION_FUEL_LIMIT,
        }
    }
}

impl TypeFunctionEvaluationState {
    pub(crate) fn enter(
        &mut self,
        scope: ScopeId,
        name: &str,
        arguments: Vec<TypeId>,
    ) -> Result<(), TypeFunctionEvaluationLimit> {
        if self
            .stack
            .iter()
            .any(|frame| frame.scope == scope && frame.name == name && frame.arguments == arguments)
        {
            return Err(TypeFunctionEvaluationLimit::RecursiveCall);
        }
        if self.stack.len() >= TYPE_FUNCTION_EVALUATION_DEPTH_LIMIT {
            return Err(TypeFunctionEvaluationLimit::DepthExceeded);
        }
        self.consume_step()?;
        self.stack.push(TypeFunctionEvaluationFrame {
            scope,
            name: name.to_owned(),
            arguments,
        });
        Ok(())
    }

    pub(crate) fn leave(&mut self) {
        self.stack.pop();
    }

    pub(crate) fn consume_step(&mut self) -> Result<(), TypeFunctionEvaluationLimit> {
        if self.remaining_fuel == 0 {
            return Err(TypeFunctionEvaluationLimit::FuelExhausted);
        }
        self.remaining_fuel -= 1;
        Ok(())
    }

    fn is_unwound(&self) -> bool {
        self.stack.is_empty()
    }
}

#[derive(Default)]
pub struct FunctionFrameState {
    pub(crate) return_stack: Vec<TypePackId>,
    pub(crate) vararg_stack: Vec<Option<TypePackId>>,
    pub(crate) parameter_expectation_stack: Vec<BTreeMap<LocalId, ParameterExpectations>>,
    pub(crate) unannotated_return_stack: Vec<bool>,
    pub(crate) contextual_return_stack: Vec<bool>,
    pub(crate) return_seen_stack: Vec<bool>,
    pub(crate) inferred_return_stack: Vec<Vec<InferredReturnPath>>,
    pub(crate) inferred_return_seed_stack: Vec<Option<TypePackId>>,
    pub(crate) function_scope_stack: Vec<ScopeId>,
    pub(crate) function_is_global_stack: Vec<bool>,
    pub(crate) global_function_stack: Vec<Option<String>>,
    pub(crate) next_local_function_id: Option<LocalId>,
    pub(crate) next_global_function_name: Option<String>,
    pub(crate) local_function_stack: Vec<Option<LocalId>>,
    pub(crate) function_has_unannotated_parameter_stack: Vec<bool>,
    pub(crate) recursive_value_call_stack: Vec<bool>,
    pub(crate) recursive_return_placeholder_stack: Vec<Option<TypeId>>,
    pub(crate) next_function_is_global: bool,
}

#[derive(Default)]
pub struct OperatorState {
    pub(crate) never_arithmetic_exprs: BTreeSet<SyntaxId>,
    pub(crate) recursive_arithmetic_exprs: BTreeSet<SyntaxId>,
}

#[derive(Default)]
pub struct CallState {
    pub(crate) discard_call_results: BTreeSet<SyntaxId>,
    pub(crate) statement_call_results: BTreeSet<SyntaxId>,
    pub(crate) infer_discarded_call_callees: BTreeSet<SyntaxId>,
    pub(crate) call_result_packs: BTreeMap<SyntaxId, TypePackId>,
}

#[derive(Default)]
pub struct RefinementState {
    pub(crate) property_probes: BTreeSet<SyntaxId>,
    pub(crate) locals: Vec<RefinementMap>,
    pub(crate) nonfallthrough_loop_assignment_snapshots: Vec<BTreeMap<TypeId, TypeKind>>,
}

#[derive(Default)]
pub struct QueryCaptureState {
    /// Function-literal parameter positions whose source queries should hide a
    /// generic call site's concrete instantiation.
    pub(crate) generic_contextual_callback_parameters: BTreeMap<SyntaxId, BTreeSet<usize>>,
    pub(crate) captured_nil_reads: BTreeMap<LocalId, Vec<CapturedNilQueryRead>>,
    /// Callback parameter locals recorded from
    /// `generic_contextual_callback_parameters`; value-flow keeps using the
    /// real local type, but source-position queries report `unknown`.
    pub(crate) generic_contextual_callback_locals: BTreeSet<LocalId>,
}

#[derive(Default)]
pub struct NilTrackingState {
    pub(crate) initialized_locals: BTreeSet<LocalId>,
    pub(crate) implicit_locals: BTreeSet<LocalId>,
    pub(crate) typeof_snapshot_locals: BTreeSet<LocalId>,
    pub(crate) guard_relaxes_to_nil_locals: BTreeSet<LocalId>,
}

impl NilTrackingState {
    pub(crate) fn local_starts_as_nil(&self, local_id: LocalId) -> bool {
        self.initialized_locals.contains(&local_id) || self.implicit_locals.contains(&local_id)
    }
}

#[derive(Default)]
pub struct LocalSurfaceState {
    pub(crate) annotated_locals: BTreeSet<LocalId>,
    pub(crate) string_format_aliases: BTreeSet<LocalId>,
    pub(crate) setmetatable_side_effect_locals: BTreeSet<LocalId>,
}

#[derive(Default)]
pub struct TableWriteState {
    pub(crate) unsealed_property_writes: BTreeMap<TypeId, BTreeMap<String, TypeId>>,
}

#[derive(Default)]
pub struct UnknownSymbolState {
    pub(crate) reported_symbols: BTreeSet<SyntaxId>,
    pub(crate) suppressed_global_reads: BTreeSet<String>,
}

pub struct ExpressionConstraintGenerator<'a> {
    pub(crate) input: GenerationInput<'a>,
    pub(crate) arena: &'a mut Arena,
    pub(crate) generated: GeneratedConstraints,
    pub(crate) next_child: BTreeMap<ScopeId, usize>,
    pub(crate) expected_by_syntax: BTreeMap<SyntaxId, TypeId>,
    pub(crate) non_ascribing_contextual_functions: BTreeSet<SyntaxId>,
    pub(crate) prebound_table_literals: BTreeMap<SyntaxId, TypeId>,
    pub(crate) alias_lowering: AliasLoweringState,
    pub(crate) function_frames: FunctionFrameState,
    pub(crate) operator: OperatorState,
    pub(crate) calls: CallState,
    pub(crate) refinements: RefinementState,
    pub(crate) query_capture: QueryCaptureState,
    pub(crate) nil_tracking: NilTrackingState,
    pub(crate) local_surface: LocalSurfaceState,
    pub(crate) table_writes: TableWriteState,
    pub(crate) unknown_symbols: UnknownSymbolState,
    pub(crate) type_function_evaluation: TypeFunctionEvaluationState,
    pub(crate) nonstrict_checked_argument_depth: usize,
    pub(crate) loop_depth: usize,
    pub(crate) repeat_guaranteed_body_depth: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ParameterExpectation {
    pub(crate) ty: TypeId,
    pub(crate) location: DiagnosticLocation,
    pub(crate) report: bool,
    pub(crate) checked_call: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ParameterExpectations {
    pub(crate) declaration_location: Option<DiagnosticLocation>,
    pub(crate) expectations: Vec<ParameterExpectation>,
}

#[derive(Clone, Copy, Debug)]
pub struct AssignmentValue {
    pub(crate) ty: TypeId,
}

#[derive(Clone, Copy, Debug)]
pub struct InferredReturnType {
    pub(crate) ty: TypeId,
    pub(crate) table_literal: bool,
    /// The returned expression carried an explicit type (a `:: T` assertion), so
    /// its type is authoritative and must not be widened in the inferred return
    /// (`return ("" :: any) :: bar<>` keeps the singleton `bar<>` resolves to).
    pub(crate) preserve: bool,
}

#[derive(Clone, Debug)]
pub struct InferredReturnPath {
    pub(crate) fixed: Vec<InferredReturnType>,
    /// Exact return pack for paths like `return f(...)`, where the call's
    /// result arity is itself inferred. `fixed` is kept as a normalized view
    /// for diagnostics and multi-path fallback.
    pub(crate) pack: Option<TypePackId>,
}

#[allow(clippy::multiple_inherent_impl)]
impl<'a> ExpressionConstraintGenerator<'a> {
    pub(crate) fn new(
        scopes: &'a ScopeTree,
        dfg: &'a DataFlowGraph,
        arena: &'a mut Arena,
        mode: AnalysisMode,
        config: GenerationConfig,
        require_return_types: &'a BTreeMap<SyntaxId, Vec<TypeId>>,
    ) -> Self {
        Self {
            input: GenerationInput {
                scopes,
                dfg,
                mode,
                config,
                require_return_types,
            },
            arena,
            generated: GeneratedConstraints::default(),
            next_child: BTreeMap::new(),
            expected_by_syntax: BTreeMap::new(),
            non_ascribing_contextual_functions: BTreeSet::new(),
            prebound_table_literals: BTreeMap::new(),
            alias_lowering: AliasLoweringState::default(),
            function_frames: FunctionFrameState::default(),
            operator: OperatorState::default(),
            calls: CallState::default(),
            refinements: RefinementState::default(),
            query_capture: QueryCaptureState::default(),
            nil_tracking: NilTrackingState::default(),
            local_surface: LocalSurfaceState::default(),
            table_writes: TableWriteState::default(),
            unknown_symbols: UnknownSymbolState::default(),
            type_function_evaluation: TypeFunctionEvaluationState::default(),
            nonstrict_checked_argument_depth: 0,
            loop_depth: 0,
            repeat_guaranteed_body_depth: 0,
        }
    }

    pub(crate) fn with_next_local_function<T>(
        &mut self,
        local_id: LocalId,
        infer: impl FnOnce(&mut Self) -> T,
    ) -> T {
        debug_assert!(self.function_frames.next_local_function_id.is_none());
        self.function_frames.next_local_function_id = Some(local_id);
        let inferred = infer(self);
        debug_assert!(self.function_frames.next_local_function_id.is_none());
        self.function_frames.next_local_function_id = None;
        inferred
    }

    pub(crate) fn with_next_global_function<T>(
        &mut self,
        global_name: String,
        infer: impl FnOnce(&mut Self) -> T,
    ) -> T {
        debug_assert!(self.function_frames.next_global_function_name.is_none());
        debug_assert!(!self.function_frames.next_function_is_global);
        self.function_frames.next_function_is_global = true;
        self.function_frames.next_global_function_name = Some(global_name);
        let inferred = infer(self);
        debug_assert!(self.function_frames.next_global_function_name.is_none());
        debug_assert!(!self.function_frames.next_function_is_global);
        self.function_frames.next_global_function_name = None;
        self.function_frames.next_function_is_global = false;
        inferred
    }

    pub(crate) fn take_pending_function_identity(
        &mut self,
    ) -> (bool, Option<LocalId>, Option<String>) {
        let function_is_global = self.function_frames.next_function_is_global;
        self.function_frames.next_function_is_global = false;
        let local_function_id = self.function_frames.next_local_function_id.take();
        let global_function_name = self.function_frames.next_global_function_name.take();
        (function_is_global, local_function_id, global_function_name)
    }

    pub(crate) fn assert_frame_stacks_empty(&self) {
        // Debug-only invariant: every per-function frame stack and depth counter
        // must unwind to empty/zero by the time the root scope closes. Collected
        // as a flat (name, is-clear) table driven by one loop so the check stays a
        // single conceptual assertion instead of ~30 separate branches.
        if !cfg!(debug_assertions) {
            return;
        }
        let frames = &self.function_frames;
        let aliases = &self.alias_lowering;
        let unwound: [(&str, bool); 29] = [
            ("type_alias_stack", aliases.type_alias_stack.is_empty()),
            (
                "type_alias_definition_stack",
                aliases.type_alias_definition_stack.is_empty(),
            ),
            ("return_stack", frames.return_stack.is_empty()),
            ("vararg_stack", frames.vararg_stack.is_empty()),
            (
                "parameter_expectation_stack",
                frames.parameter_expectation_stack.is_empty(),
            ),
            (
                "unannotated_return_stack",
                frames.unannotated_return_stack.is_empty(),
            ),
            (
                "contextual_return_stack",
                frames.contextual_return_stack.is_empty(),
            ),
            ("return_seen_stack", frames.return_seen_stack.is_empty()),
            (
                "inferred_return_stack",
                frames.inferred_return_stack.is_empty(),
            ),
            (
                "inferred_return_seed_stack",
                frames.inferred_return_seed_stack.is_empty(),
            ),
            (
                "function_scope_stack",
                frames.function_scope_stack.is_empty(),
            ),
            (
                "function_is_global_stack",
                frames.function_is_global_stack.is_empty(),
            ),
            (
                "global_function_stack",
                frames.global_function_stack.is_empty(),
            ),
            (
                "local_function_stack",
                frames.local_function_stack.is_empty(),
            ),
            (
                "function_has_unannotated_parameter_stack",
                frames.function_has_unannotated_parameter_stack.is_empty(),
            ),
            (
                "recursive_value_call_stack",
                frames.recursive_value_call_stack.is_empty(),
            ),
            (
                "recursive_return_placeholder_stack",
                frames.recursive_return_placeholder_stack.is_empty(),
            ),
            (
                "generic_type_substitutions",
                aliases.generic_type_substitutions.is_empty(),
            ),
            (
                "generic_type_pack_substitutions",
                aliases.generic_type_pack_substitutions.is_empty(),
            ),
            (
                "nonfallthrough_loop_assignment_snapshots",
                self.refinements
                    .nonfallthrough_loop_assignment_snapshots
                    .is_empty(),
            ),
            (
                "next_local_function_id",
                frames.next_local_function_id.is_none(),
            ),
            (
                "next_global_function_name",
                frames.next_global_function_name.is_none(),
            ),
            ("next_function_is_global", !frames.next_function_is_global),
            (
                "type_alias_function_depth",
                aliases.type_alias_function_depth == 0,
            ),
            (
                "generic_alias_type_argument_depth",
                aliases.generic_alias_type_argument_depth == 0,
            ),
            (
                "type_function_evaluation",
                self.type_function_evaluation.is_unwound(),
            ),
            (
                "nonstrict_checked_argument_depth",
                self.nonstrict_checked_argument_depth == 0,
            ),
            ("loop_depth", self.loop_depth == 0),
            (
                "repeat_guaranteed_body_depth",
                self.repeat_guaranteed_body_depth == 0,
            ),
        ];
        for (name, is_unwound) in unwound {
            assert!(
                is_unwound,
                "{name} must be clear when the root scope closes"
            );
        }
    }

    pub(crate) fn bind_function_parameter_expected_type(
        &mut self,
        expr: &Expr,
        expected: TypeId,
    ) -> bool {
        if self.input.mode == AnalysisMode::Nonstrict && self.nonstrict_checked_argument_depth == 0
        {
            return false;
        }
        let Expr::Local { local, .. } = expr else {
            return false;
        };
        let Some(binding) = self.input.scopes.lookup_local_id(local.id) else {
            return false;
        };
        if binding.kind != ValueBindingKind::FunctionParameter {
            return false;
        }
        let Some(def) = self.input.dfg.local(local.id) else {
            return false;
        };
        let parameter = self.input.dfg.get(def).ty;
        if !self.local_type_can_bind_expected(parameter) {
            return false;
        }
        let expectations = self.function_parameter_expected_parts(expected);
        let location = DiagnosticLocation::from_opt(expr.location());
        let mut recorded = false;
        for expected in expectations {
            if !self.expected_type_can_bind_local(expected)
                || self.type_contains_type(parameter, expected, &mut BTreeSet::new())
            {
                continue;
            }
            self.record_function_parameter_expectation(local.id, expected, location, true);
            recorded = true;
        }
        recorded
    }
    pub(crate) fn bind_function_parameter_property_read_expectation(
        &mut self,
        expr: &Expr,
        name: &str,
        value: TypeId,
    ) -> bool {
        if self.input.mode == AnalysisMode::Nonstrict && self.nonstrict_checked_argument_depth == 0
        {
            return false;
        }
        let Some(local_id) = self.local_from_grouped_expr(expr) else {
            return false;
        };
        let Some(binding) = self.input.scopes.lookup_local_id(local_id) else {
            return false;
        };
        if binding.kind != ValueBindingKind::FunctionParameter {
            return false;
        }
        let Some(def) = self.input.dfg.local(local_id) else {
            return false;
        };
        if !self.local_type_can_bind_expected(self.input.dfg.get(def).ty) {
            return false;
        }
        let mut table = TableType::new(TableState::Free);
        table
            .properties
            .insert(name.to_owned(), TableProperty::read_only(value));
        let expected = self.arena.alloc(TypeKind::Table(table));
        let location = DiagnosticLocation::from_opt(expr.location());
        self.record_function_parameter_expectation(local_id, expected, location, false);
        true
    }
    fn record_function_parameter_expectation(
        &mut self,
        local_id: LocalId,
        expected: TypeId,
        location: DiagnosticLocation,
        report: bool,
    ) {
        let expectation = ParameterExpectation {
            ty: expected,
            location,
            report,
            checked_call: self.nonstrict_checked_argument_depth > 0,
        };
        if let Some(expectations) = self
            .function_frames
            .parameter_expectation_stack
            .last_mut()
            .and_then(|parameters| parameters.get_mut(&local_id))
        {
            expectations.expectations.push(expectation);
        } else {
            let Some(def) = self.input.dfg.local(local_id) else {
                return;
            };
            let parameter = self.input.dfg.get(def).ty;
            self.generated
                .constraints
                .push(Constraint::unify(parameter, expected));
        }
    }
    fn function_parameter_expected_parts(&self, expected: TypeId) -> Vec<TypeId> {
        match self.arena.get(self.arena.follow(expected)) {
            TypeKind::Intersection(options) => options.clone(),
            _ => vec![expected],
        }
    }
    pub(crate) fn bind_function_parameter_indexer_expected_type(
        &mut self,
        expr: &Expr,
        key: TypeId,
        value: TypeId,
    ) -> bool {
        if !self.expected_type_can_bind_local(key) || !self.expected_type_can_bind_local(value) {
            return false;
        }
        let Expr::Local { local, .. } = expr else {
            return false;
        };
        let has_existing_scalar_bound = self
            .function_frames
            .parameter_expectation_stack
            .last()
            .and_then(|parameters| parameters.get(&local.id))
            .is_some_and(|expectations| {
                expectations.expectations.iter().any(|expectation| {
                    self.expected_type_is_scalarish(expectation.ty, &mut BTreeSet::new())
                })
            });
        if !has_existing_scalar_bound {
            return false;
        }
        let mut table = TableType::new(TableState::Sealed);
        table.indexer = Some(TableIndexer {
            key,
            value,
            read_only: false,
        });
        let expected = self.arena.alloc(TypeKind::Table(table));
        self.bind_function_parameter_expected_type(expr, expected)
    }
    pub(crate) fn resolve_function_parameter_expectations(
        &mut self,
        parameter_expectations: &BTreeMap<LocalId, ParameterExpectations>,
    ) {
        for (local_id, parameter_expectations) in parameter_expectations {
            let expectations = &parameter_expectations.expectations;
            let Some(def) = self.input.dfg.local(*local_id) else {
                continue;
            };
            let parameter = self.input.dfg.get(def).ty;
            if expectations.is_empty() || !self.local_type_can_bind_expected(parameter) {
                continue;
            }
            let unique = self.unique_expected_types(expectations);
            if unique.len() == 1 {
                if expectations.iter().all(|expectation| !expectation.report) {
                    continue;
                }
                if !self.type_contains_type(parameter, unique[0], &mut BTreeSet::new()) {
                    self.bind_or_unify_function_parameter_expectation(parameter, unique[0]);
                }
                continue;
            }
            if let Some(extern_ty) = self.extern_expectation_satisfying_tables(&unique) {
                if !self.type_contains_type(parameter, extern_ty, &mut BTreeSet::new()) {
                    self.bind_or_unify_function_parameter_expectation(parameter, extern_ty);
                }
                continue;
            }
            if self.expected_types_reduce_parameter_to_never(&unique) {
                self.bind_parameter_to_never(parameter);
                self.report_parameter_reduced_to_never(
                    *local_id,
                    parameter_expectations.declaration_location,
                    expectations,
                );
                continue;
            }
            let intersection = self.arena.alloc(TypeKind::Intersection(unique.clone()));
            let simplified = simplify_type(self.arena, intersection);
            if self.is_never_type(simplified)
                || self.type_has_uninhabited_property(simplified, &mut BTreeSet::new())
            {
                self.bind_parameter_to_never(parameter);
                self.report_parameter_reduced_to_never(
                    *local_id,
                    parameter_expectations.declaration_location,
                    expectations,
                );
            } else if self.type_contains_type(parameter, simplified, &mut BTreeSet::new()) {
                continue;
            } else {
                self.bind_or_unify_function_parameter_expectation(parameter, simplified);
            }
        }
    }
    fn bind_or_unify_function_parameter_expectation(
        &mut self,
        parameter: TypeId,
        expected: TypeId,
    ) {
        let parameter = self.arena.follow(parameter);
        if matches!(
            self.arena.get(parameter),
            TypeKind::Table(table) if table.state == TableState::Free
        ) {
            self.arena.replace(parameter, TypeKind::Bound(expected));
            return;
        }
        self.generated
            .constraints
            .push(Constraint::unify(parameter, expected));
    }
    pub(crate) fn function_parameter_expected_argument_type(
        &self,
        expr: &Expr,
        expected: TypeId,
    ) -> Option<TypeId> {
        if self.input.mode == AnalysisMode::Nonstrict && self.nonstrict_checked_argument_depth == 0
        {
            return None;
        }
        let Expr::Local { local, .. } = expr else {
            return None;
        };
        let binding = self.input.scopes.lookup_local_id(local.id)?;
        if binding.kind != ValueBindingKind::FunctionParameter {
            return None;
        }
        let def = self.input.dfg.local(local.id)?;
        if !self.local_type_can_bind_expected(self.input.dfg.get(def).ty)
            || !self.expected_type_can_bind_local(expected)
        {
            return None;
        }
        Some(expected)
    }
    pub(crate) fn settle_function_parameter_surface(
        &mut self,
        ty: TypeId,
        protected_returns: TypePackId,
    ) {
        let ty = self.arena.follow(ty);
        let TypeKind::Table(mut table) = self.arena.get(ty).clone() else {
            return;
        };
        let mut changed = false;
        let unknown = self.primitives().unknown;
        for property in table.properties.values_mut() {
            if property.read_only
                && !property.write_only
                && self.is_settleable_parameter_read_property(property.ty)
                && !self.pack_contains_type(
                    property.ty,
                    protected_returns,
                    &mut BTreeSet::new(),
                    &mut BTreeSet::new(),
                )
            {
                property.ty = unknown;
                changed = true;
            }
        }
        if changed {
            self.arena.replace(ty, TypeKind::Table(table));
        }
    }
    fn is_unconstrained_free_type(&self, ty: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(ty)),
            TypeKind::Free(variable)
                if variable.lower_bound.is_none() && variable.upper_bound.is_none()
        )
    }
    fn is_settleable_parameter_read_property(&self, ty: TypeId) -> bool {
        self.is_unconstrained_free_type(ty)
            || matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Function(_))
    }
    pub(crate) fn report_parameter_reduced_to_never(
        &mut self,
        local_id: LocalId,
        declaration_location: Option<DiagnosticLocation>,
        expectations: &[ParameterExpectation],
    ) {
        let name = self
            .input
            .scopes
            .lookup_local_id(local_id)
            .map(|binding| binding.name.as_str())
            .unwrap_or("<parameter>");
        let location = declaration_location
            .or_else(|| expectations.first().map(|expectation| expectation.location))
            .unwrap_or_else(DiagnosticLocation::missing);
        let reduced = TypeDiagnostic::error(DiagnosticCategory::Constraint, location).with_typed(
            crate::diagnostic::Payload::ParameterReducedToNever {
                parameter: name.to_owned(),
            },
        );
        self.generated.diagnostics.push(reduced);
        let report_expectations_are_checked_scalarish = expectations.iter().all(|expectation| {
            expectation.checked_call
                && self.expected_type_is_scalarish(expectation.ty, &mut BTreeSet::new())
        });
        if report_expectations_are_checked_scalarish {
            return;
        }
        for expectation in expectations {
            let required = self.arena.summary(expectation.ty);
            let diagnostic =
                TypeDiagnostic::error(DiagnosticCategory::Constraint, expectation.location)
                    .with_typed(crate::diagnostic::Payload::ParameterRequiredSubtype {
                        parameter: name.to_owned(),
                        required,
                    });
            self.generated.diagnostics.push(diagnostic);
        }
    }
    pub(crate) fn local_type_can_bind_expected(&self, local_ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(local_ty)) {
            TypeKind::Free(_) => true,
            TypeKind::Table(table) => table.state == TableState::Free,
            _ => false,
        }
    }
    fn bind_parameter_to_never(&mut self, parameter: TypeId) {
        let parameter = self.arena.follow(parameter);
        match self.arena.get(parameter) {
            TypeKind::Free(_) => self.bind_free_to(parameter, self.primitives().never),
            TypeKind::Table(table) if table.state == TableState::Free => {
                self.arena
                    .replace(parameter, TypeKind::Bound(self.primitives().never));
            }
            _ => {}
        }
    }
    pub(crate) fn expected_type_can_bind_local(&self, expected: TypeId) -> bool {
        self.expected_type_is_scalarish(expected, &mut BTreeSet::new())
            || matches!(
                self.arena.get(self.arena.follow(expected)),
                TypeKind::Table(_) | TypeKind::Metatable { .. }
            )
    }
    fn unique_expected_types(&self, expectations: &[ParameterExpectation]) -> Vec<TypeId> {
        let mut unique = Vec::new();
        for expectation in expectations {
            let expected = self.arena.follow(expectation.ty);
            if unique.iter().any(|existing| {
                let existing = self.arena.follow(*existing);
                existing == expected || self.arena.get(existing) == self.arena.get(expected)
            }) {
                continue;
            }
            unique.push(expected);
        }
        unique
    }
    fn expected_types_reduce_parameter_to_never(&self, expected: &[TypeId]) -> bool {
        let has_scalarish = expected
            .iter()
            .any(|ty| self.expected_type_is_scalarish(*ty, &mut BTreeSet::new()));
        let has_tableish = expected.iter().any(|ty| {
            matches!(
                self.arena.get(self.arena.follow(*ty)),
                TypeKind::Table(_) | TypeKind::Metatable { .. }
            )
        });
        has_scalarish && has_tableish
    }
    /// When one expected type is an extern class that exposes every property
    /// the co-present table/metatable expectations require (by name or via an
    /// indexer), the intersection of the expectations is inhabited by that
    /// extern, so the parameter must be bound to it rather than collapsed to
    /// `never`. Returns that extern type. A property-name check is used because
    /// the expected tables' property types are still-unsolved inference
    /// variables at this point, so full subtyping would spuriously fail.
    fn extern_expectation_satisfying_tables(&self, expected: &[TypeId]) -> Option<TypeId> {
        let required = expected
            .iter()
            .flat_map(|ty| self.table_required_property_names(*ty))
            .collect::<Vec<_>>();
        if required.is_empty() {
            return None;
        }
        expected.iter().copied().find(|ty| {
            let TypeKind::Extern {
                properties,
                indexer,
                ..
            } = self.arena.get(self.arena.follow(*ty))
            else {
                return false;
            };
            required
                .iter()
                .all(|name| properties.contains_key(name) || indexer.is_some())
        })
    }
    /// Property names a table/metatable expectation requires of a value.
    fn table_required_property_names(&self, ty: TypeId) -> Vec<String> {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty) {
            TypeKind::Table(table) => table.properties.keys().cloned().collect(),
            TypeKind::Metatable { table, .. } => {
                let table = self.arena.follow(*table);
                match self.arena.get(table) {
                    TypeKind::Table(table) => table.properties.keys().cloned().collect(),
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }
    fn type_has_uninhabited_property(&self, ty: TypeId, seen: &mut BTreeSet<TypeId>) -> bool {
        let ty = self.arena.follow(ty);
        if !seen.insert(ty) {
            return false;
        }
        match self.arena.get(ty) {
            TypeKind::Table(table) => table
                .properties
                .values()
                .any(|property| self.is_never_type(property.ty)),
            TypeKind::Metatable { table, .. } => self.type_has_uninhabited_property(*table, seen),
            TypeKind::Intersection(options) => options
                .iter()
                .any(|option| self.type_has_uninhabited_property(*option, seen)),
            TypeKind::Bound(bound) => self.type_has_uninhabited_property(*bound, seen),
            _ => false,
        }
    }
    pub(crate) fn expected_type_is_scalarish(
        &self,
        expected: TypeId,
        seen: &mut BTreeSet<TypeId>,
    ) -> bool {
        let expected = self.arena.follow(expected);
        if !seen.insert(expected) {
            return false;
        }
        match self.arena.get(expected) {
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Never
            | TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Error
            | TypeKind::Blocked(_)
            | TypeKind::Extern { .. } => true,
            TypeKind::Union(types) | TypeKind::Intersection(types) => types
                .iter()
                .all(|ty| self.expected_type_is_scalarish(*ty, seen)),
            TypeKind::Negation(ty) => self.expected_type_is_scalarish(*ty, seen),
            TypeKind::Bound(ty) => self.expected_type_is_scalarish(*ty, seen),
            TypeKind::Free(_)
            | TypeKind::Generic(_)
            | TypeKind::Function(_)
            | TypeKind::Table(_)
            | TypeKind::Metatable { .. }
            | TypeKind::TypeFunctionInstance { .. } => false,
        }
    }
    pub(crate) fn expected_accepts_without_subtype(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> bool {
        if !self.contains_dynamic_type(expected, &mut BTreeSet::new()) {
            return false;
        }
        matches!(
            (
                self.arena.get(self.arena.follow(actual)),
                self.arena.get(self.arena.follow(expected)),
            ),
            (TypeKind::Table(_), TypeKind::Table(_))
                | (TypeKind::Function(_), TypeKind::Function(_))
        )
    }
    pub(crate) fn literal_discriminator(
        &self,
        items: &[TableItem],
    ) -> Option<(String, SingletonType)> {
        items.iter().find_map(|item| {
            let property = match (&item.kind, &item.key) {
                (JsonTableItemKind::Record, Some(Expr::String { value, .. }))
                | (JsonTableItemKind::General, Some(Expr::String { value, .. })) => value.clone(),
                (JsonTableItemKind::Record, Some(Expr::Global { name, .. })) => {
                    name.as_str().to_owned()
                }
                _ => return None,
            };
            let singleton = match &item.value {
                Expr::String { value, .. } => SingletonType::String(value.clone()),
                Expr::Bool { value, .. } => SingletonType::Boolean(*value),
                _ => return None,
            };
            Some((property, singleton))
        })
    }
    pub(crate) fn bind_free_to(&mut self, candidate: TypeId, target: TypeId) {
        let candidate = self.arena.follow(candidate);
        let target = self.arena.follow(target);
        if candidate == target
            || !matches!(self.arena.get(candidate), TypeKind::Free(_))
            || matches!(self.arena.get(target), TypeKind::Free(_))
            || self.type_contains_type(candidate, target, &mut BTreeSet::new())
        {
            return;
        }
        self.arena.replace(candidate, TypeKind::Bound(target));
    }
    pub(crate) fn widen_mutable_literal_type(&self, ty: TypeId) -> TypeId {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Singleton(SingletonType::Boolean(_)) => self.primitives().boolean,
            TypeKind::Singleton(SingletonType::String(_)) => self.primitives().string,
            _ => ty,
        }
    }
    pub(crate) fn widen_mutable_query_type(&self, ty: TypeId) -> TypeId {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Singleton(_) => self.widen_mutable_literal_type(ty),
            TypeKind::Union(options) => {
                let mut primitive = None;
                for option in options {
                    let option = self.arena.follow(*option);
                    let current = match self.arena.get(option) {
                        TypeKind::Singleton(singleton) => singleton.primitive(),
                        TypeKind::Primitive(primitive) => *primitive,
                        _ => return ty,
                    };
                    if primitive.is_some_and(|primitive| primitive != current) {
                        return ty;
                    }
                    primitive = Some(current);
                }
                primitive
                    .map(|primitive| match primitive {
                        PrimitiveType::Boolean => self.primitives().boolean,
                        PrimitiveType::String => self.primitives().string,
                        PrimitiveType::Nil
                        | PrimitiveType::Number
                        | PrimitiveType::Thread
                        | PrimitiveType::Buffer
                        | PrimitiveType::Vector => ty,
                    })
                    .unwrap_or(ty)
            }
            _ => ty,
        }
    }
    pub(crate) fn materialize_unsealed_property_writes_in_type(&mut self, ty: TypeId) {
        self.materialize_unsealed_property_writes_in_type_inner(
            ty,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        );
    }
    fn materialize_unsealed_property_writes_in_type_inner(
        &mut self,
        ty: TypeId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return;
        }

        if let Some(writes) = self.table_writes.unsealed_property_writes.get(&ty).cloned()
            && let TypeKind::Table(mut table) = self.arena.get(ty).clone()
            && matches!(table.state, TableState::Free | TableState::Unsealed)
        {
            for (name, value) in writes {
                table
                    .properties
                    .entry(name)
                    .or_insert_with(|| TableProperty::new(value));
            }
            self.arena.replace(ty, TypeKind::Table(table));
        }

        match self.arena.get(ty).clone() {
            TypeKind::Function(function) => {
                self.materialize_unsealed_property_writes_in_pack_inner(
                    function.arguments,
                    seen_types,
                    seen_packs,
                );
                self.materialize_unsealed_property_writes_in_pack_inner(
                    function.returns,
                    seen_types,
                    seen_packs,
                );
            }
            TypeKind::Table(table) => {
                for ty in table.instantiated_type_params {
                    self.materialize_unsealed_property_writes_in_type_inner(
                        ty, seen_types, seen_packs,
                    );
                }
                for property in table.properties.values() {
                    self.materialize_unsealed_property_writes_in_type_inner(
                        property.ty,
                        seen_types,
                        seen_packs,
                    );
                }
                if let Some(indexer) = table.indexer {
                    self.materialize_unsealed_property_writes_in_type_inner(
                        indexer.key,
                        seen_types,
                        seen_packs,
                    );
                    self.materialize_unsealed_property_writes_in_type_inner(
                        indexer.value,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.materialize_unsealed_property_writes_in_type_inner(
                    table, seen_types, seen_packs,
                );
                self.materialize_unsealed_property_writes_in_type_inner(
                    metatable, seen_types, seen_packs,
                );
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => {
                for ty in arguments {
                    self.materialize_unsealed_property_writes_in_type_inner(
                        ty, seen_types, seen_packs,
                    );
                }
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.materialize_unsealed_property_writes_in_type_inner(
                    inner, seen_types, seen_packs,
                );
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => {}
        }
    }
    fn materialize_unsealed_property_writes_in_pack_inner(
        &mut self,
        pack: TypePackId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return;
        }
        match self.arena.get_pack(pack).clone() {
            TypePackKind::List { types, tail } => {
                for ty in types {
                    self.materialize_unsealed_property_writes_in_type_inner(
                        ty, seen_types, seen_packs,
                    );
                }
                if let Some(tail) = tail {
                    self.materialize_unsealed_property_writes_in_pack_inner(
                        tail, seen_types, seen_packs,
                    );
                }
            }
            TypePackKind::Variadic { ty } => {
                self.materialize_unsealed_property_writes_in_type_inner(ty, seen_types, seen_packs);
            }
            TypePackKind::Bound(bound) => self
                .materialize_unsealed_property_writes_in_pack_inner(bound, seen_types, seen_packs),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => {}
        }
    }
    pub(crate) fn type_contains_type(
        &self,
        needle: TypeId,
        haystack: TypeId,
        seen: &mut BTreeSet<TypeId>,
    ) -> bool {
        let haystack = self.arena.follow(haystack);
        if needle == haystack {
            return true;
        }
        if !seen.insert(haystack) {
            return false;
        }
        match self.arena.get(haystack) {
            TypeKind::Function(function) => {
                self.pack_contains_type(needle, function.arguments, seen, &mut BTreeSet::new())
                    || self.pack_contains_type(needle, function.returns, seen, &mut BTreeSet::new())
            }
            TypeKind::Table(table) => {
                table
                    .properties
                    .values()
                    .any(|property| self.type_contains_type(needle, property.ty, seen))
                    || table.indexer.as_ref().is_some_and(|indexer| {
                        self.type_contains_type(needle, indexer.key, seen)
                            || self.type_contains_type(needle, indexer.value, seen)
                    })
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_contains_type(needle, *table, seen)
                    || self.type_contains_type(needle, *metatable, seen)
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => arguments
                .iter()
                .any(|ty| self.type_contains_type(needle, *ty, seen)),
            TypeKind::Negation(ty) | TypeKind::Bound(ty) => {
                self.type_contains_type(needle, *ty, seen)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }
    fn pack_contains_type(
        &self,
        needle: TypeId,
        haystack: TypePackId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let haystack = self.arena.follow_pack(haystack);
        if !seen_packs.insert(haystack) {
            return false;
        }
        match self.arena.get_pack(haystack) {
            TypePackKind::List { types, tail } => {
                types
                    .iter()
                    .any(|ty| self.type_contains_type(needle, *ty, seen_types))
                    || tail.is_some_and(|tail| {
                        self.pack_contains_type(needle, tail, seen_types, seen_packs)
                    })
            }
            TypePackKind::Variadic { ty } => self.type_contains_type(needle, *ty, seen_types),
            TypePackKind::Bound(pack) => {
                self.pack_contains_type(needle, *pack, seen_types, seen_packs)
            }
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }
    pub(crate) fn type_contains_free_or_generic(
        &self,
        ty: TypeId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return false;
        }
        match self.arena.get(ty) {
            TypeKind::Free(_) | TypeKind::Generic(_) => true,
            TypeKind::Function(function) => {
                self.pack_contains_free_or_generic(function.arguments, seen_types, seen_packs)
                    || self.pack_contains_free_or_generic(function.returns, seen_types, seen_packs)
            }
            TypeKind::Table(table) => {
                table
                    .instantiated_type_params
                    .iter()
                    .any(|ty| self.type_contains_free_or_generic(*ty, seen_types, seen_packs))
                    || table.instantiated_type_pack_params.iter().any(|pack| {
                        self.pack_contains_free_or_generic(*pack, seen_types, seen_packs)
                    })
                    || table.properties.values().any(|property| {
                        self.type_contains_free_or_generic(property.ty, seen_types, seen_packs)
                            || property.write_ty.is_some_and(|ty| {
                                self.type_contains_free_or_generic(ty, seen_types, seen_packs)
                            })
                    })
                    || table.indexer.as_ref().is_some_and(|indexer| {
                        self.type_contains_free_or_generic(indexer.key, seen_types, seen_packs)
                            || self.type_contains_free_or_generic(
                                indexer.value,
                                seen_types,
                                seen_packs,
                            )
                    })
            }
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                properties.values().any(|property| {
                    self.type_contains_free_or_generic(property.ty, seen_types, seen_packs)
                        || property.write_ty.is_some_and(|ty| {
                            self.type_contains_free_or_generic(ty, seen_types, seen_packs)
                        })
                }) || indexer.as_ref().is_some_and(|indexer| {
                    self.type_contains_free_or_generic(indexer.key, seen_types, seen_packs)
                        || self.type_contains_free_or_generic(indexer.value, seen_types, seen_packs)
                })
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_contains_free_or_generic(*table, seen_types, seen_packs)
                    || self.type_contains_free_or_generic(*metatable, seen_types, seen_packs)
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => arguments
                .iter()
                .any(|ty| self.type_contains_free_or_generic(*ty, seen_types, seen_packs)),
            TypeKind::Negation(ty) | TypeKind::Bound(ty) => {
                self.type_contains_free_or_generic(*ty, seen_types, seen_packs)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }
    fn pack_contains_free_or_generic(
        &self,
        pack: TypePackId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack) {
            TypePackKind::Free { .. } | TypePackKind::Generic(_) => true,
            TypePackKind::List { types, tail } => {
                types
                    .iter()
                    .any(|ty| self.type_contains_free_or_generic(*ty, seen_types, seen_packs))
                    || tail.is_some_and(|tail| {
                        self.pack_contains_free_or_generic(tail, seen_types, seen_packs)
                    })
            }
            TypePackKind::Variadic { ty } => {
                self.type_contains_free_or_generic(*ty, seen_types, seen_packs)
            }
            TypePackKind::Bound(bound) => {
                self.pack_contains_free_or_generic(*bound, seen_types, seen_packs)
            }
            TypePackKind::Error => false,
        }
    }
    pub(crate) fn report_unknown_symbol(
        &mut self,
        syntax_id: SyntaxId,
        symbol: &str,
        location: DiagnosticLocation,
    ) {
        if self.unknown_symbols.reported_symbols.insert(syntax_id) {
            self.generated
                .diagnostics
                .push(TypeDiagnostic::unknown_symbol(symbol, location));
        }
    }
    pub(crate) fn with_suppressed_unknown_global<T>(
        &mut self,
        symbol: &str,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let inserted = self
            .unknown_symbols
            .suppressed_global_reads
            .insert(symbol.to_owned());
        let result = f(self);
        if inserted {
            self.unknown_symbols.suppressed_global_reads.remove(symbol);
        }
        result
    }
    pub(crate) fn contains_dynamic_type(&self, ty: TypeId, seen: &mut BTreeSet<TypeId>) -> bool {
        let ty = self.arena.follow(ty);
        if !seen.insert(ty) {
            return false;
        }
        match self.arena.get(ty) {
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => true,
            TypeKind::Function(function) => {
                self.contains_dynamic_pack(function.arguments, seen, &mut BTreeSet::new())
                    || self.contains_dynamic_pack(function.returns, seen, &mut BTreeSet::new())
            }
            TypeKind::Table(table) => {
                table
                    .properties
                    .values()
                    .any(|property| self.contains_dynamic_type(property.ty, seen))
                    || table.indexer.as_ref().is_some_and(|indexer| {
                        self.contains_dynamic_type(indexer.key, seen)
                            || self.contains_dynamic_type(indexer.value, seen)
                    })
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.contains_dynamic_type(*table, seen)
                    || self.contains_dynamic_type(*metatable, seen)
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => arguments
                .iter()
                .any(|ty| self.contains_dynamic_type(*ty, seen)),
            TypeKind::Negation(ty) | TypeKind::Bound(ty) => self.contains_dynamic_type(*ty, seen),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Free(_)
            | TypeKind::Generic(_)
            | TypeKind::Never => false,
        }
    }
    fn contains_dynamic_pack(
        &self,
        pack: TypePackId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack) {
            TypePackKind::List { types, tail } => {
                types
                    .iter()
                    .any(|ty| self.contains_dynamic_type(*ty, seen_types))
                    || tail.is_some_and(|tail| {
                        self.contains_dynamic_pack(tail, seen_types, seen_packs)
                    })
            }
            TypePackKind::Variadic { ty } => self.contains_dynamic_type(*ty, seen_types),
            TypePackKind::Bound(pack) => self.contains_dynamic_pack(*pack, seen_types, seen_packs),
            TypePackKind::Error => true,
            TypePackKind::Free { .. } | TypePackKind::Generic(_) => false,
        }
    }
    pub(crate) fn expr_is_function_parameter_local(&self, expr: &Expr) -> bool {
        let Expr::Local { local, .. } = expr else {
            return false;
        };
        if local.luau_type.is_some() {
            return false;
        }
        self.input
            .scopes
            .lookup_local_id(local.id)
            .is_some_and(|binding| binding.kind == ValueBindingKind::FunctionParameter)
    }
    pub(crate) fn expr_is_unannotated_function_parameter_path(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Local { .. } => self.expr_is_function_parameter_local(expr),
            Expr::IndexName { expr, .. } | Expr::IndexExpr { expr, .. } => {
                self.expr_is_unannotated_function_parameter_path(expr)
            }
            Expr::Group { expr, .. } => self.expr_is_unannotated_function_parameter_path(expr),
            _ => false,
        }
    }
    pub(crate) fn dfg_type_for_expr(&mut self, expr: &Expr) -> TypeId {
        self.input
            .dfg
            .expression(expr.syntax_id())
            .map(|def| self.input.dfg.get(def).ty)
            .unwrap_or_else(|| self.recovery_type_at(expr.location(), "missing expression def"))
    }
    pub(crate) fn local_type(&mut self, local: &Local) -> TypeId {
        self.input
            .dfg
            .local(local.id)
            .map(|def| self.input.dfg.get(def).ty)
            .unwrap_or_else(|| self.recovery_type_at(local.location, "missing local def"))
    }
    pub(crate) fn local_annotation_or_free(&mut self, scope: ScopeId, local: &Local) -> TypeId {
        let local_ty = self.local_type(local);
        if let Some(annotation) = &local.luau_type {
            self.local_surface.annotated_locals.insert(local.id);
            let annotation_ty =
                self.with_generic_alias_type_arguments(|this| this.lower_type(scope, annotation));
            self.expect_type(local.location, local_ty, annotation_ty);
            self.bind_free_to(local_ty, annotation_ty);
            annotation_ty
        } else {
            local_ty
        }
    }
    /// Nil acceptance used while generating expression-flow constraints.
    ///
    /// This runs before every constraint has been solved, so free and blocked
    /// placeholders are kept non-nil rather than being treated like optional
    /// arity holes. Post-solve call/overload arity checks use
    /// `member_access::type_accepts_nil_for_arity`, whose policy is wider.
    pub(crate) fn type_accepts_nil(&self, ty: TypeId, seen: &mut BTreeSet<TypeId>) -> bool {
        let ty = self.arena.follow(ty);
        if !seen.insert(ty) {
            return false;
        }
        match self.arena.get(ty) {
            TypeKind::Primitive(PrimitiveType::Nil) => true,
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error => true,
            TypeKind::Union(options) => options
                .iter()
                .any(|option| self.type_accepts_nil(*option, seen)),
            TypeKind::Intersection(options) => options
                .iter()
                .all(|option| self.type_accepts_nil(*option, seen)),
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Function(_)
            | TypeKind::Table(_)
            | TypeKind::Metatable { .. }
            | TypeKind::Extern { .. }
            | TypeKind::TypeFunctionInstance { .. }
            | TypeKind::Negation(_)
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Never => false,
        }
    }
    pub(crate) fn expr_is_function_parameter(&self, expr: &Expr) -> bool {
        let Expr::Local { local, .. } = expr else {
            return false;
        };
        self.input
            .scopes
            .lookup_local_id(local.id)
            .is_some_and(|binding| binding.kind == ValueBindingKind::FunctionParameter)
    }
    pub(crate) fn truthy_part(&mut self, ty: TypeId) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Primitive(PrimitiveType::Nil)
            | TypeKind::Singleton(SingletonType::Boolean(false)) => self.primitives().never,
            TypeKind::Primitive(PrimitiveType::Boolean) => self
                .arena
                .alloc(TypeKind::Singleton(SingletonType::Boolean(true))),
            TypeKind::Singleton(SingletonType::Boolean(true)) => ty,
            TypeKind::Singleton(SingletonType::String(_)) => ty,
            TypeKind::Unknown => self.truthy_unknown_type(),
            TypeKind::Generic(_) => {
                let truthy = self.truthy_unknown_type();
                self.arena.alloc(TypeKind::Intersection(vec![ty, truthy]))
            }
            TypeKind::Union(types) => {
                let truthy = types
                    .into_iter()
                    .map(|ty| self.truthy_part(ty))
                    .collect::<Vec<_>>();
                self.union_type(truthy)
            }
            _ => ty,
        }
    }
    fn truthy_unknown_type(&mut self) -> TypeId {
        let false_ty = self
            .arena
            .alloc(TypeKind::Singleton(SingletonType::Boolean(false)));
        let falsey = self.union_type(vec![self.primitives().nil, false_ty]);
        self.arena.alloc(TypeKind::Negation(falsey))
    }
    pub(crate) fn falsy_part(&mut self, ty: TypeId) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Primitive(PrimitiveType::Nil)
            | TypeKind::Singleton(SingletonType::Boolean(false)) => ty,
            TypeKind::Primitive(PrimitiveType::Boolean) => self
                .arena
                .alloc(TypeKind::Singleton(SingletonType::Boolean(false))),
            TypeKind::Free(_) => self.primitives().nil,
            TypeKind::Generic(_) => self.generic_falsy_type(ty),
            TypeKind::Union(types) => {
                let falsy = types
                    .into_iter()
                    .map(|ty| self.falsy_part(ty))
                    .collect::<Vec<_>>();
                self.union_type(falsy)
            }
            _ => self.primitives().never,
        }
    }
    fn generic_falsy_type(&mut self, ty: TypeId) -> TypeId {
        let false_ty = self
            .arena
            .alloc(TypeKind::Singleton(SingletonType::Boolean(false)));
        let falsey = self.union_type(vec![self.primitives().nil, false_ty]);
        self.arena.alloc(TypeKind::Intersection(vec![ty, falsey]))
    }
    pub(crate) fn nonnil_part(&mut self, ty: TypeId) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Primitive(PrimitiveType::Nil) => {
                let negated = self.arena.alloc(TypeKind::Negation(self.primitives().nil));
                self.raw_intersection_type(vec![ty, negated])
            }
            TypeKind::Union(types) => {
                let nonnil = types
                    .into_iter()
                    .map(|ty| {
                        if self.arena.is_nil(ty) {
                            self.primitives().never
                        } else {
                            self.nonnil_part(ty)
                        }
                    })
                    .collect::<Vec<_>>();
                self.union_type(nonnil)
            }
            TypeKind::Any => self.primitives().any,
            TypeKind::Unknown | TypeKind::Blocked(_) => {
                self.arena.alloc(TypeKind::Negation(self.primitives().nil))
            }
            TypeKind::Free(_) | TypeKind::Generic(_) => {
                let negated = self.arena.alloc(TypeKind::Negation(self.primitives().nil));
                self.intersection_type(vec![ty, negated])
            }
            _ => ty,
        }
    }
    pub(crate) fn nil_part(&mut self, ty: TypeId) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Primitive(PrimitiveType::Nil) => ty,
            TypeKind::Union(types) => {
                if types.iter().any(|ty| self.arena.is_nil(*ty)) {
                    return self.primitives().nil;
                }
                let nil = types
                    .into_iter()
                    .map(|ty| self.nil_part(ty))
                    .collect::<Vec<_>>();
                self.union_type(nil)
            }
            TypeKind::Any | TypeKind::Unknown | TypeKind::Blocked(_) => self.primitives().nil,
            TypeKind::Free(_) | TypeKind::Generic(_) => {
                self.raw_intersection_type(vec![ty, self.primitives().nil])
            }
            _ => self.raw_intersection_type(vec![ty, self.primitives().nil]),
        }
    }
    pub(crate) fn union_type(&mut self, types: Vec<TypeId>) -> TypeId {
        let never = self.primitives().never;
        let mut flattened = Vec::new();
        for ty in types {
            let ty = self.arena.follow(ty);
            if ty == never {
                continue;
            }
            match self.arena.get(ty).clone() {
                TypeKind::Union(options) => flattened.extend(options),
                _ => flattened.push(ty),
            }
        }
        flattened.sort_unstable();
        flattened.dedup();
        let primitives = self.primitives();
        let has_boolean = flattened.contains(&primitives.boolean);
        let has_string = flattened.contains(&primitives.string);
        if has_boolean || has_string {
            flattened.retain(|ty| match self.arena.get(*ty) {
                TypeKind::Singleton(SingletonType::Boolean(_)) => !has_boolean,
                TypeKind::Singleton(SingletonType::String(_)) => !has_string,
                _ => true,
            });
        }
        match flattened.as_slice() {
            [] => never,
            [only] => *only,
            _ => self.arena.alloc(TypeKind::Union(flattened)),
        }
    }
    pub(crate) fn intersection_type(&mut self, types: Vec<TypeId>) -> TypeId {
        let mut flattened = Vec::new();
        for ty in types {
            let ty = self.arena.follow(ty);
            match self.arena.get(ty).clone() {
                TypeKind::Intersection(options) => flattened.extend(options),
                _ => flattened.push(ty),
            }
        }
        flattened.sort_unstable();
        flattened.dedup();
        match flattened.as_slice() {
            [] => self.primitives().never,
            [only] => *only,
            _ => {
                let intersection = self.arena.alloc(TypeKind::Intersection(flattened));
                simplify_type(self.arena, intersection)
            }
        }
    }
    pub(crate) fn raw_intersection_type(&mut self, types: Vec<TypeId>) -> TypeId {
        let mut flattened = Vec::new();
        for ty in types {
            let ty = self.arena.follow(ty);
            match self.arena.get(ty).clone() {
                TypeKind::Intersection(options) => flattened.extend(options),
                _ => flattened.push(ty),
            }
        }
        flattened.sort_unstable();
        flattened.dedup();
        match flattened.as_slice() {
            [] => self.primitives().never,
            [only] => *only,
            _ => self.arena.alloc(TypeKind::Intersection(flattened)),
        }
    }
    pub(crate) fn primitives(&self) -> PrimitiveTypes {
        self.arena.primitives()
    }
    pub(crate) fn refined_local_type(&self, local_id: LocalId) -> Option<TypeId> {
        let key = RefinementKey::Symbol(Symbol::Local(local_id));
        self.refined_type(&key)
    }
    pub(crate) fn refined_type(&self, key: &RefinementKey) -> Option<TypeId> {
        self.refinements
            .locals
            .iter()
            .rev()
            .find_map(|refinements| refinements.get(key).copied())
    }
    pub(crate) fn refine_current_local(&mut self, local_id: LocalId, ty: TypeId) {
        if let Some(refinements) = self.refinements.locals.last_mut() {
            let key = RefinementKey::Symbol(Symbol::Local(local_id));
            refinements.insert(key, ty);
        }
    }
    pub(crate) fn merge_current_refinements(&mut self, refinements: RefinementMap) {
        if let Some(current) = self.refinements.locals.last_mut() {
            current.extend(refinements);
        } else if !refinements.is_empty() {
            self.refinements.locals.push(refinements);
        }
    }
    pub(crate) fn enter_child(&mut self, parent: ScopeId) -> ScopeId {
        let next = self.next_child.entry(parent).or_default();
        let scope = self
            .input
            .scopes
            .get(parent)
            .children
            .get(*next)
            .copied()
            .unwrap_or(parent);
        *next += 1;
        scope
    }
    pub(crate) fn local_is_captured_upvalue(&self, scope: ScopeId, local_id: LocalId) -> bool {
        let Some(function_scope) = self.function_frames.function_scope_stack.last().copied() else {
            return false;
        };
        let Some(local_scope) = self.input.scopes.local_definition_scope(scope, local_id) else {
            return false;
        };
        !self
            .input
            .scopes
            .is_descendant_or_same(local_scope, function_scope)
    }
}
