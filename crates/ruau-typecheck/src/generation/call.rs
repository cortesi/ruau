//! Call-site type inference for expression constraint generation.
//!
//! Owns how a call expression infers its result and argument types: return
//! pack inference, argument/parameter binding, explicit type instantiation,
//! the typed builtin call surfaces (table.insert/create/find/unpack, string
//! pattern returns), generic-call checks, and inferred-return generalization.

use std::collections::BTreeSet;

use ruau_syntax::{Expr, IndexOp, Location, SyntaxId, TypeParameter};

use crate::{
    ast_util::ungroup_expr,
    call_pack::{ExpectedCallParameterPack, ReceiverParameter},
    constraints::{Constraint, ConstraintSolveError},
    diagnostics::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Payload},
    generalize::{Instantiator, function_signature_has_callback_free_correlation},
    generation::{
        expression::{
            CallArgumentSupply, explicit_table_builtin_method, is_string_global,
            is_table_clone_call, is_table_freeze_call, is_table_insert_call, lua_pattern_captures,
            setmetatable_call_metamethod_function_needing_annotation, string_lib_method,
            string_literal,
        },
        state::{ExpressionConstraintGenerator, InferredReturnPath, InferredReturnType},
        string_format::{self, FormatArgument},
        type_function_eval::TypeFunctionEvaluation,
    },
    graph::Mode,
    scopes::{ScopeId, TypeBindingKind},
    subtype::{SubtypeErrorKind, SubtypeTarget, Subtyper},
    type_function::SETMETATABLE_TYPE_FUNCTION,
    types::{
        FunctionType, PrimitiveType, SingletonType, TableIndexer, TableState, TableType, TypeId,
        TypeKind, TypeLevel, TypePackId, TypePackKind, TypePackTail, is_top_function_type,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectStart {
    Count,
    From(isize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinalCallArgumentExpansion {
    NoValues,
    Tail(TypePackId),
}

#[derive(Clone, Debug)]
struct SelectArgumentValues {
    fixed: Vec<TypeId>,
    tail: Option<TypePackTail>,
}

/// Pads a builtin's known return types out to the caller's requested slot
/// count: known types first, then `pad` (`Some(nil)` for nil-padded
/// builtins, `None` where the tail is unknown).
fn padded_return_types(
    types: impl IntoIterator<Item = TypeId>,
    pad: Option<TypeId>,
    target_count: usize,
) -> Vec<Option<TypeId>> {
    types
        .into_iter()
        .map(Some)
        .chain(std::iter::repeat(pad))
        .take(target_count)
        .collect()
}

#[allow(clippy::multiple_inherent_impl)]
impl<'a> ExpressionConstraintGenerator<'a> {
    fn resolved_expected_callee(
        &mut self,
        scope: ScopeId,
        func: &Expr,
        callee: TypeId,
        type_arguments: &[TypeParameter],
        location: Option<Location>,
    ) -> TypeId {
        let expected = self.callable_type(scope, func, callee);
        if type_arguments.is_empty() {
            return expected;
        }
        if let Some(method) = explicit_table_builtin_method(func) {
            self.explicit_table_builtin_instantiation(scope, method, type_arguments, location)
        } else {
            self.explicit_type_instantiation(scope, expected, type_arguments, location)
        }
    }

    pub(crate) fn call_return_values(
        &mut self,
        scope: ScopeId,
        value: &Expr,
        target_count: usize,
        expected_types: &[Option<TypeId>],
    ) -> Option<Vec<Option<TypeId>>> {
        let value = ungroup_expr(value);
        let Expr::Call {
            location,
            func,
            type_arguments,
            args,
            is_self,
            ..
        } = value
        else {
            return None;
        };
        if let Some(types) =
            self.require_return_types_call(scope, value, *location, func, args, target_count)
        {
            return Some(types);
        }
        if let Some(result) = self.builtin_global_call_return_values(
            scope,
            value,
            *location,
            func,
            args,
            target_count,
        ) {
            return result;
        }
        if let Some(types) =
            self.string_pattern_call_return_values(scope, value, *location, target_count)
        {
            return Some(types);
        }
        let callee = self.expr_type(scope, func);
        if let Some(types) = self.degenerate_callee_call_return_values(
            scope,
            value,
            *location,
            func,
            args,
            callee,
            target_count,
        ) {
            return Some(types);
        }
        self.check_nilable_callee(callee, *location);
        let expected_callee =
            self.resolved_expected_callee(scope, func, callee, type_arguments, *location);
        if self.is_error_type(expected_callee) {
            for arg in args {
                self.expr_type(scope, arg);
            }
            let error = self.primitives().error;
            let expr_ty = self.dfg_type_for_expr(value);
            self.bind_actual(*location, value.syntax_id(), expr_ty, error);
            return Some(vec![Some(error); target_count]);
        }
        if let Some(types) = self.select_call_return_values(
            scope,
            value,
            *location,
            func,
            args,
            *is_self,
            expected_callee,
            target_count,
        ) {
            return Some(types);
        }
        if let Some(return_types) = self.generic_pack_call_return_values(
            scope,
            value,
            *location,
            func,
            args,
            *is_self,
            callee,
            expected_callee,
            target_count,
        ) {
            return Some(return_types);
        }
        let return_types = self.function_fixed_return_types(expected_callee)?;
        let (arg_types, arg_tail, checked_callee) =
            self.call_argument_types(scope, func, args, *is_self, expected_callee);
        let supplied = CallArgumentSupply::from_parts(arg_types.len(), arg_tail);
        if !*is_self && is_table_clone_call(func) {
            let result = self.table_clone_result_type(&arg_types, arg_tail);
            let expr_ty = self.dfg_type_for_expr(value);
            self.bind_actual(*location, value.syntax_id(), expr_ty, result);
            let nil = self.primitives().nil;
            return Some(padded_return_types([result], Some(nil), target_count));
        }
        self.bind_missing_free_call_arguments_to_nil(
            expected_callee,
            supplied.count_for_missing_bindings(),
        );
        let arguments = self.pack_with_tail(arg_types, arg_tail);
        if self.expr_is_function_parameter(func) {
            self.bind_free_callee_to_function(callee, arguments, None);
        }
        let constraint_callee = if self.expr_is_function_parameter(func) {
            callee
        } else {
            checked_callee
        };
        let call_location = func.location().map(DiagnosticLocation::from);
        let arity_mismatch =
            self.report_too_few_call_arguments(expected_callee, supplied, call_location);
        let callee_is_generic = self.function_is_generic(expected_callee);
        let return_shape_can_use_expected_guidance = return_types
            .iter()
            .any(|ty| !self.type_cannot_use_expected_guidance(*ty));
        let substituted_returns = if callee_is_generic && !return_types.is_empty() {
            let expr_ty = self.dfg_type_for_expr(value);
            let mut substituted = Vec::with_capacity(return_types.len());
            substituted.push(expr_ty);
            for _ in 1..return_types.len() {
                substituted.push(self.placeholder_free_type("generic-call-return"));
            }
            if return_shape_can_use_expected_guidance {
                for (substituted, expected) in substituted
                    .iter()
                    .copied()
                    .zip(expected_types.iter().copied())
                {
                    if let Some(expected) = expected
                        && !self.is_dynamic(expected)
                    {
                        self.bind_free_to(substituted, expected);
                    }
                }
            }
            Some(substituted)
        } else {
            None
        };
        let expected_returns_pack = substituted_returns
            .as_ref()
            .map(|returns| self.pack(returns.clone()));
        if !arity_mismatch {
            self.generated.constraints.push(Constraint::call(
                constraint_callee,
                arguments,
                self.input.mode == Mode::Nonstrict,
                args.iter()
                    .map(|arg| arg.location().map(DiagnosticLocation::from))
                    .collect(),
                expected_returns_pack,
                call_location,
                true,
            ));
        }
        if let Some(substituted) = substituted_returns.as_ref() {
            self.record_actual(*location, value.syntax_id(), substituted[0]);
        } else if let Some(first) = return_types.first().copied() {
            let expr_ty = self.dfg_type_for_expr(value);
            self.bind_actual(*location, value.syntax_id(), expr_ty, first);
        }
        let return_types = substituted_returns.unwrap_or(return_types);
        // `string.find`/`string.match` with extra arguments (init/plain) keep
        // their normal argument diagnostics, but their result arity still
        // depends on the literal pattern's captures.
        let return_types = if let Some(method) = string_lib_method(func)
            && matches!(method, "find" | "match")
            && args.len() > 2
            && let Some(pattern) = string_literal(&args[1])
            && let Some(captures) = lua_pattern_captures(pattern)
        {
            if method == "find" {
                let optional_number = self.optional_number_type();
                let mut pack = vec![optional_number, optional_number];
                pack.extend(self.pattern_capture_types(&captures, false));
                pack
            } else {
                self.pattern_capture_types(&captures, true)
            }
        } else {
            return_types
        };
        if self.input.mode == Mode::Strict
            && !return_types.is_empty()
            && return_types.len() < target_count
        {
            self.generated.diagnostics.push(
                Diagnostic::error(
                    DiagnosticCategory::TypePack,
                    DiagnosticLocation::from_opt(*location),
                )
                .with_typed(Payload::ArityMismatch {
                    counts: Some(crate::diagnostics::ArityCounts {
                        expected: target_count,
                        actual: return_types.len(),
                    }),
                    subtype: crate::diagnostics::SubtypeContext::default(),
                }),
            );
        }
        let nil = self.primitives().nil;
        Some(padded_return_types(return_types, Some(nil), target_count))
    }

    /// Returns the host-supplied required return types for this call site,
    /// when the input pins them (after still typing the callee and arguments).
    fn require_return_types_call(
        &mut self,
        scope: ScopeId,
        value: &Expr,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
        target_count: usize,
    ) -> Option<Vec<Option<TypeId>>> {
        let return_types = self
            .input
            .require_return_types
            .get(&value.syntax_id())
            .cloned()?;
        self.expr_type(scope, func);
        for arg in args {
            self.expr_type(scope, arg);
        }
        if let Some(first) = return_types.first().copied() {
            let expr_ty = self.dfg_type_for_expr(value);
            self.bind_actual(location, value.syntax_id(), expr_ty, first);
        }
        let nil = self.primitives().nil;
        Some(padded_return_types(return_types, Some(nil), target_count))
    }

    /// Infers the return pack for the iteration/protected-call builtins
    /// (`ipairs`, `next`, `pairs`, `pcall`/`xpcall`). The outer `Option`
    /// distinguishes "not one of these builtins" (`None`, keep matching) from
    /// "handled" (`Some`), whose inner value is the resolved pack or `None`
    /// when the call cannot produce one.
    fn builtin_global_call_return_values(
        &mut self,
        scope: ScopeId,
        value: &Expr,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
        target_count: usize,
    ) -> Option<Option<Vec<Option<TypeId>>>> {
        let Expr::Global { name, .. } = func else {
            return None;
        };
        match name.as_str() {
            "ipairs" => {
                self.expr_type(scope, func);
                let arg_ty = args
                    .first()
                    .map(|arg| self.expr_type(scope, arg))
                    .unwrap_or_else(|| self.primitives().any);
                for arg in args.iter().skip(1) {
                    self.expr_type(scope, arg);
                }
                let (_, value_ty) = self
                    .for_in_table_iteration_types(arg_ty)
                    .unwrap_or((self.primitives().number, self.primitives().any));
                let iterator = self.ipairs_iterator_type(arg_ty, value_ty);
                let expr_ty = self.dfg_type_for_expr(value);
                self.bind_actual(location, value.syntax_id(), expr_ty, iterator);
                Some(Some(padded_return_types(
                    [iterator, arg_ty, self.primitives().number],
                    None,
                    target_count,
                )))
            }
            "next" => {
                self.expr_type(scope, func);
                let arg_ty = args
                    .first()
                    .map(|arg| self.expr_type(scope, arg))
                    .unwrap_or_else(|| self.primitives().any);
                for arg in args.iter().skip(1) {
                    self.expr_type(scope, arg);
                }
                let (key, table_value) = self
                    .builtin_table_iteration_types(arg_ty, false)
                    .unwrap_or((self.primitives().any, self.primitives().any));
                let key = self.optional_type(key);
                if let Some(first) = [key, table_value].first().copied() {
                    let expr_ty = self.dfg_type_for_expr(value);
                    self.bind_actual(location, value.syntax_id(), expr_ty, first);
                }
                let nil = self.primitives().nil;
                Some(Some(padded_return_types(
                    [key, table_value],
                    Some(nil),
                    target_count,
                )))
            }
            "pairs" => {
                self.expr_type(scope, func);
                let arg_ty = args
                    .first()
                    .map(|arg| self.expr_type(scope, arg))
                    .unwrap_or_else(|| self.primitives().any);
                for arg in args.iter().skip(1) {
                    self.expr_type(scope, arg);
                }
                let Some((key, table_value)) = self
                    .builtin_table_iteration_types(arg_ty, true)
                    .or_else(|| self.dynamic_pairs_call_iteration_types(arg_ty))
                else {
                    return Some(None);
                };
                let state = self.pairs_state_type(arg_ty);
                let iterator = self.pairs_iterator_type(state, key, table_value);
                let expr_ty = self.dfg_type_for_expr(value);
                self.bind_actual(location, value.syntax_id(), expr_ty, iterator);
                Some(Some(padded_return_types(
                    [iterator, state, self.primitives().nil],
                    None,
                    target_count,
                )))
            }
            "pcall" | "xpcall" => {
                let protected = args.first()?;
                self.expr_type(scope, func);
                let protected_ty = self.expr_type(scope, protected);
                for arg in args.iter().skip(1) {
                    self.expr_type(scope, arg);
                }
                let mut return_types = vec![self.primitives().boolean];
                let Some(fixed) = self.function_fixed_return_types(protected_ty) else {
                    return Some(None);
                };
                return_types.extend(fixed);
                if let Some(first) = return_types.first().copied() {
                    let expr_ty = self.dfg_type_for_expr(value);
                    self.bind_actual(location, value.syntax_id(), expr_ty, first);
                }
                let nil = self.primitives().nil;
                Some(Some(padded_return_types(
                    return_types,
                    Some(nil),
                    target_count,
                )))
            }
            _ => None,
        }
    }

    /// Infers the return pack for a `string` library pattern call (e.g.
    /// `string.match`) whose literal pattern fixes its capture arity.
    fn string_pattern_call_return_values(
        &mut self,
        scope: ScopeId,
        value: &Expr,
        location: Option<Location>,
        target_count: usize,
    ) -> Option<Vec<Option<TypeId>>> {
        let return_types = self.string_pattern_call_return_types(scope, value)?;
        if self.input.mode == Mode::Strict
            && !return_types.is_empty()
            && return_types.len() < target_count
        {
            self.generated.diagnostics.push(
                Diagnostic::error(
                    DiagnosticCategory::TypePack,
                    DiagnosticLocation::from_opt(location),
                )
                .with_typed(Payload::ArityMismatch {
                    counts: Some(crate::diagnostics::ArityCounts {
                        expected: target_count,
                        actual: return_types.len(),
                    }),
                    subtype: crate::diagnostics::SubtypeContext::default(),
                }),
            );
        }
        if let Some(first) = return_types.first().copied() {
            let expr_ty = self.dfg_type_for_expr(value);
            self.bind_actual(location, value.syntax_id(), expr_ty, first);
        }
        let nil = self.primitives().nil;
        Some(padded_return_types(return_types, Some(nil), target_count))
    }

    /// Handles calls whose callee resolves to `never` or to a top-function
    /// refinement: still type the arguments, then collapse the result pack to
    /// `never`/`*error-type*` respectively.
    #[allow(clippy::too_many_arguments)]
    fn degenerate_callee_call_return_values(
        &mut self,
        scope: ScopeId,
        value: &Expr,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
        callee: TypeId,
        target_count: usize,
    ) -> Option<Vec<Option<TypeId>>> {
        if self.is_never_type(callee) {
            for arg in args {
                self.expr_type(scope, arg);
            }
            let never = self.primitives().never;
            let expr_ty = self.dfg_type_for_expr(value);
            self.bind_actual(location, value.syntax_id(), expr_ty, never);
            return Some(vec![Some(never); target_count]);
        }
        if self.callee_is_top_function_refinement(func, callee) {
            for arg in args {
                self.expr_type(scope, arg);
            }
            self.report_top_function_refinement_call(func.location());
            let error = self.primitives().error;
            let expr_ty = self.dfg_type_for_expr(value);
            self.bind_actual(location, value.syntax_id(), expr_ty, error);
            return Some(vec![Some(error); target_count]);
        }
        None
    }

    /// Infers the return pack for a `select(...)` call when its argument
    /// sequence is statically recoverable. Argument types and missing-argument
    /// nil bindings are still established even when no pack can be produced.
    #[allow(clippy::too_many_arguments)]
    fn select_call_return_values(
        &mut self,
        scope: ScopeId,
        value: &Expr,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
        is_self: bool,
        expected_callee: TypeId,
        target_count: usize,
    ) -> Option<Vec<Option<TypeId>>> {
        if is_self || !is_select_global(func) {
            return None;
        }
        let (arg_types, arg_tail, _) =
            self.call_argument_types(scope, func, args, is_self, expected_callee);
        self.bind_missing_free_call_arguments_to_nil(
            expected_callee,
            arg_types.len() + usize::from(arg_tail.is_some()),
        );
        let return_types =
            self.select_return_types(scope, args, &arg_types, arg_tail, target_count)?;
        if let Some(first) = return_types.first().copied() {
            let expr_ty = self.dfg_type_for_expr(value);
            self.bind_actual(location, value.syntax_id(), expr_ty, first);
        }
        Some(return_types.into_iter().map(Some).collect())
    }

    #[allow(clippy::too_many_arguments)]
    fn generic_pack_call_return_values(
        &mut self,
        scope: ScopeId,
        value: &Expr,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
        is_self: bool,
        callee: TypeId,
        expected_callee: TypeId,
        target_count: usize,
    ) -> Option<Vec<Option<TypeId>>> {
        if target_count == 0 || !self.function_returns_own_generic_pack(expected_callee) {
            return None;
        }

        let expr_ty = self.dfg_type_for_expr(value);
        let mut return_types = Vec::with_capacity(target_count);
        return_types.push(expr_ty);
        for _ in 1..target_count {
            return_types.push(self.placeholder_free_type("generic-pack-call-return"));
        }

        let (arg_types, arg_tail, checked_callee) =
            self.call_argument_types(scope, func, args, is_self, expected_callee);
        let supplied = CallArgumentSupply::from_parts(arg_types.len(), arg_tail);
        self.bind_missing_free_call_arguments_to_nil(
            expected_callee,
            supplied.count_for_missing_bindings(),
        );
        let arguments = self.pack_with_tail(arg_types, arg_tail);
        if self.expr_is_function_parameter(func) {
            self.bind_free_callee_to_function(callee, arguments, None);
        }
        let constraint_callee = if self.expr_is_function_parameter(func) {
            callee
        } else {
            checked_callee
        };
        let call_location = func.location().map(DiagnosticLocation::from);
        let arity_mismatch =
            self.report_too_few_call_arguments(expected_callee, supplied, call_location);
        if !arity_mismatch {
            let tail = self.arena.alloc_pack(TypePackKind::Variadic {
                ty: self.primitives().any,
            });
            let expected_returns = self.pack_with_tail(return_types.clone(), Some(tail));
            self.generated.constraints.push(Constraint::call(
                constraint_callee,
                arguments,
                self.input.mode == Mode::Nonstrict,
                args.iter()
                    .map(|arg| arg.location().map(DiagnosticLocation::from))
                    .collect(),
                Some(expected_returns),
                call_location,
                true,
            ));
        }
        self.record_actual(location, value.syntax_id(), expr_ty);
        Some(return_types.into_iter().map(Some).collect())
    }
    /// Computes the return types of `string.find`/`string.match`/`string.gmatch`
    /// calls whose pattern is a string literal, deriving the capture count from
    /// the pattern. Returns `None` for non-matching calls or malformed patterns,
    /// leaving normal call typing to handle them.
    fn string_pattern_call_return_types(
        &mut self,
        scope: ScopeId,
        value: &Expr,
    ) -> Option<Vec<TypeId>> {
        let Expr::Call { func, args, .. } = value else {
            return None;
        };
        // `string.gmatch(s, pattern)()` — the iterator yields one value per
        // capture, or the whole match when the pattern has no captures.
        if args.is_empty()
            && let Expr::Call {
                func: inner_func,
                args: inner_args,
                is_self: inner_is_self,
                ..
            } = ungroup_expr(func)
            && let Some(("gmatch", pattern)) =
                string_pattern_call(inner_func, inner_args, *inner_is_self)
            && let Some(captures) = lua_pattern_captures(pattern)
        {
            self.expr_type(scope, func);
            return Some(self.pattern_capture_types(&captures, true));
        }
        // `string.find(s, pattern)` / `string.match(s, pattern)`.
        if let Expr::Call { is_self, .. } = value
            && let Some((method, pattern)) = string_pattern_call(func, args, *is_self)
            && matches!(method, "find" | "match")
            && args.len() == if *is_self { 1 } else { 2 }
            && let Some(captures) = lua_pattern_captures(pattern)
        {
            self.expr_type(scope, func);
            for arg in args {
                self.expr_type(scope, arg);
            }
            return Some(if method == "find" {
                let optional_number = self.optional_number_type();
                let mut pack = vec![optional_number, optional_number];
                pack.extend(self.pattern_capture_types(&captures, false));
                pack
            } else {
                self.pattern_capture_types(&captures, true)
            });
        }
        None
    }

    fn optional_number_type(&mut self) -> TypeId {
        let primitives = self.primitives();
        self.union_type(vec![primitives.nil, primitives.number])
    }

    fn optional_string_type(&mut self) -> TypeId {
        let primitives = self.primitives();
        self.union_type(vec![primitives.nil, primitives.string])
    }

    pub(crate) fn optional_type(&mut self, ty: TypeId) -> TypeId {
        self.union_type(vec![self.primitives().nil, ty])
    }

    fn pattern_capture_types(&mut self, captures: &[bool], default_match: bool) -> Vec<TypeId> {
        let optional_number = self.optional_number_type();
        let optional_string = self.optional_string_type();
        if captures.is_empty() && default_match {
            return vec![optional_string];
        }
        captures
            .iter()
            .map(|&is_position| {
                if is_position {
                    optional_number
                } else {
                    optional_string
                }
            })
            .collect()
    }
    pub(crate) fn call_argument_types(
        &mut self,
        scope: ScopeId,
        func: &Expr,
        args: &[Expr],
        is_self: bool,
        expected_callee: TypeId,
    ) -> (Vec<TypeId>, Option<TypePackId>, TypeId) {
        let original_expected_callee = expected_callee;
        let receiver = if is_self && matches!(func, Expr::IndexName { .. }) {
            ReceiverParameter::Supplied
        } else {
            ReceiverParameter::Explicit
        };
        let (expected_callee, bind_expected_parameters) =
            self.instantiate_expected_call_callee(expected_callee);
        let bind_self_method_parameters =
            is_self_method_call_through_self(func) && self.function_has_self(expected_callee);
        let aggregate_checked_errors = self.nonstrict_checked_argument_rules_apply(expected_callee);
        let expected_parameters =
            ExpectedCallParameterPack::from_callee(self.arena, expected_callee, receiver);
        let original_expected_parameters =
            ExpectedCallParameterPack::from_callee(self.arena, original_expected_callee, receiver);
        let mut arg_types = Vec::new();
        let mut arg_tail = None;
        let mut argument_mismatches = Vec::new();
        if is_self && let Expr::IndexName { expr: receiver, .. } = func {
            let expected = expected_parameters.receiver();
            let checked_expected = expected.map(|expected| {
                self.reduce_call_argument_expected_type(
                    scope,
                    expected,
                    receiver.location().map(DiagnosticLocation::from),
                )
            });
            let cached_actual = self
                .generated
                .queries
                .actual_by_syntax(receiver.syntax_id());
            let actual = if let Some(actual) = cached_actual {
                // Typing the IndexName callee already typed its receiver. Re-entering a
                // receiver recursively regenerates every constraint in a chained self call.
                // Apply the expected relation to the recorded type without regenerating it.
                self.apply_expected_to_typed_expr(
                    receiver,
                    actual,
                    checked_expected,
                    aggregate_checked_errors,
                )
            } else {
                self.expr_type_with_checked_call_expected(
                    scope,
                    receiver,
                    checked_expected,
                    aggregate_checked_errors,
                )
            };
            if (bind_expected_parameters || bind_self_method_parameters)
                && let Some(expected) = expected
            {
                self.bind_call_expected_parameter(actual, expected);
            }
            let expected = checked_expected;
            if let Some(expected) = expected {
                self.bind_scalar_call_argument_expected_type_if_needed(receiver, actual, expected);
                self.push_concrete_call_argument_subtype_if_needed(receiver, actual, expected);
                if !aggregate_checked_errors
                    && let Some(diagnostic) =
                        self.call_argument_expected_mismatch_diagnostic(receiver, actual, expected)
                {
                    let defer =
                        self.call_argument_mismatch_needs_deferred_diagnostic(actual, expected);
                    argument_mismatches.push((diagnostic, defer));
                }
            }
            arg_types.push(
                expected
                    .filter(|expected| self.expected_accepts_without_subtype(actual, *expected))
                    .unwrap_or(actual),
            );
        }
        for (index, arg) in args.iter().enumerate() {
            if index + 1 == args.len()
                && let Some(expansion) = self.final_call_argument_expansion(scope, arg)
            {
                match expansion {
                    FinalCallArgumentExpansion::NoValues => {}
                    FinalCallArgumentExpansion::Tail(tail) => arg_tail = Some(tail),
                }
                continue;
            }
            let expected = expected_parameters.parameter_at(self.arena, index);
            let original_expected = original_expected_parameters.parameter_at(self.arena, index);
            let checked_expected = expected.map(|expected| {
                self.reduce_call_argument_expected_type(
                    scope,
                    expected,
                    arg.location().map(DiagnosticLocation::from),
                )
            });
            if bind_expected_parameters && let Some(original_expected) = original_expected {
                self.mark_generic_contextual_callback_query_parameters(arg, original_expected);
            }
            let actual = self.expr_type_with_checked_call_expected(
                scope,
                arg,
                checked_expected,
                aggregate_checked_errors,
            );
            if (bind_expected_parameters || bind_self_method_parameters)
                && let Some(expected) = expected
            {
                self.bind_call_expected_parameter(actual, expected);
            }
            let expected = checked_expected;
            if let Some(expected) = expected {
                self.bind_scalar_call_argument_expected_type_if_needed(arg, actual, expected);
                self.push_concrete_call_argument_subtype_if_needed(arg, actual, expected);
                if !aggregate_checked_errors
                    && let Some(diagnostic) =
                        self.call_argument_expected_mismatch_diagnostic(arg, actual, expected)
                {
                    let defer =
                        self.call_argument_mismatch_needs_deferred_diagnostic(actual, expected);
                    argument_mismatches.push((diagnostic, defer));
                }
            }
            let argument_ty = expected
                .and_then(|expected| self.function_parameter_expected_argument_type(arg, expected))
                .or_else(|| {
                    expected
                        .filter(|expected| self.expected_accepts_without_subtype(actual, *expected))
                })
                .unwrap_or(actual);
            arg_types.push(argument_ty);
        }
        let defer_sibling_mismatches = argument_mismatches.len() > 1;
        self.generated.deferred_diagnostics.extend(
            argument_mismatches.into_iter().enumerate().filter_map(
                |(index, (diagnostic, defer))| {
                    (defer || (defer_sibling_mismatches && index > 0)).then_some(diagnostic)
                },
            ),
        );
        let checked_callee = self
            .reduce_call_callee_parameter_type_functions(scope, expected_callee)
            .unwrap_or(if bind_expected_parameters {
                expected_callee
            } else {
                original_expected_callee
            });
        (arg_types, arg_tail, checked_callee)
    }

    fn mark_generic_contextual_callback_query_parameters(
        &mut self,
        arg: &Expr,
        original_expected: TypeId,
    ) {
        if !matches!(arg, Expr::Function { .. }) {
            return;
        }
        let query_parameters =
            self.generic_contextual_callback_query_parameter_indices(original_expected);
        if query_parameters.is_empty() {
            return;
        }
        self.query_capture
            .generic_contextual_callback_parameters
            .entry(arg.syntax_id())
            .or_default()
            .extend(query_parameters);
    }

    fn generic_contextual_callback_query_parameter_indices(
        &self,
        original_expected: TypeId,
    ) -> BTreeSet<usize> {
        let TypeKind::Function(function) = self.arena.get(self.arena.follow(original_expected))
        else {
            return BTreeSet::new();
        };
        self.arena
            .normalize_pack(function.arguments)
            .types
            .into_iter()
            .enumerate()
            .filter_map(|(index, ty)| {
                self.type_contains_free_or_generic(ty, &mut BTreeSet::new(), &mut BTreeSet::new())
                    .then_some(index)
            })
            .collect()
    }

    fn call_argument_mismatch_needs_deferred_diagnostic(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> bool {
        let TypeKind::Function(actual_function) = self.arena.get(self.arena.follow(actual)) else {
            return false;
        };
        if !is_top_function_type(self.arena, actual_function) {
            return false;
        }
        matches!(
            self.arena.get(self.arena.follow(expected)),
            TypeKind::Function(expected_function)
                if (!expected_function.generics.is_empty()
                    || !expected_function.generic_packs.is_empty())
                    && !is_top_function_type(self.arena, expected_function)
        )
    }

    fn call_argument_expected_mismatch_diagnostic(
        &self,
        arg: &Expr,
        actual: TypeId,
        expected: TypeId,
    ) -> Option<Diagnostic> {
        if self.is_dynamic(actual)
            || self.is_error_type(expected)
            || self.expected_accepts_without_subtype(actual, expected)
            || self.nonstrict_union_expected_subtype_is_permissive(actual, expected)
        {
            return None;
        }
        let error = Subtyper::new(self.arena)
            .is_subtype(actual, expected)
            .err()?;
        let mut diagnostic =
            ConstraintSolveError::Subtype(error).into_diagnostic_with_arena(Some(&*self.arena));
        diagnostic.primary_location = DiagnosticLocation::from_opt(arg.location());
        Some(diagnostic)
    }

    fn final_call_argument_expansion(
        &mut self,
        scope: ScopeId,
        arg: &Expr,
    ) -> Option<FinalCallArgumentExpansion> {
        if matches!(ungroup_expr(arg), Expr::Varargs { .. }) {
            // A trailing `...` spreads the enclosing function's variadic pack
            // into the call as a tail, not a single value, so forwarding it to a
            // fixed-arity callee (`f(a, b, ...)`) supplies any number of extra
            // arguments that are simply ignored rather than a spurious arity
            // mismatch.
            let pack = self
                .function_frames
                .vararg_stack
                .last()
                .and_then(|pack| *pack)?;
            self.expr_type(scope, arg);
            return Some(FinalCallArgumentExpansion::Tail(pack));
        }
        if self.call_fixed_return_count(scope, arg) == Some(0) {
            let _ = self.call_return_values(scope, arg, 0, &[]);
            return Some(FinalCallArgumentExpansion::NoValues);
        }
        if let Some(tail) = self.call_variadic_return_pack(scope, arg) {
            self.expr_type(scope, arg);
            return Some(FinalCallArgumentExpansion::Tail(tail));
        }
        self.free_call_result_pack(scope, arg, "call-argument-returns")
            .map(FinalCallArgumentExpansion::Tail)
    }

    fn select_return_types(
        &mut self,
        scope: ScopeId,
        args: &[Expr],
        arg_types: &[TypeId],
        arg_tail: Option<TypePackId>,
        target_count: usize,
    ) -> Option<Vec<TypeId>> {
        if target_count == 0 {
            return Some(Vec::new());
        }
        let Some(start) = select_start_argument(args.first()) else {
            return self.dynamic_select_return_types(args.first(), target_count);
        };
        if start == SelectStart::Count {
            return Some(self.pad_select_returns(
                vec![self.primitives().number],
                &None,
                target_count,
            ));
        }

        let SelectStart::From(start) = start else {
            unreachable!("count case returned above");
        };
        let values = self.select_argument_values(scope, args, arg_types, arg_tail);
        let start = select_start_index(start, values.fixed.len())?;
        let returns = values.fixed.into_iter().skip(start).collect::<Vec<_>>();
        Some(self.pad_select_returns(returns, &values.tail, target_count))
    }

    fn dynamic_select_return_types(
        &self,
        start_arg: Option<&Expr>,
        target_count: usize,
    ) -> Option<Vec<TypeId>> {
        let start_arg = ungroup_expr(start_arg?);
        if matches!(start_arg, Expr::String { .. }) {
            return None;
        }
        Some(vec![self.primitives().any; target_count])
    }

    fn select_argument_values(
        &mut self,
        scope: ScopeId,
        args: &[Expr],
        arg_types: &[TypeId],
        arg_tail: Option<TypePackId>,
    ) -> SelectArgumentValues {
        let mut fixed = Vec::new();
        for (arg_type_index, (index, arg)) in (1..).zip(args.iter().enumerate().skip(1)) {
            let is_last = index + 1 == args.len();
            let last_arg_is_call = is_last && matches!(ungroup_expr(arg), Expr::Call { .. });
            if last_arg_is_call {
                if let Some(return_types) = self.fixed_return_types_from_call_argument(scope, arg) {
                    if self.input.mode == Mode::Nonstrict
                        && return_types.iter().all(|ty| self.is_dynamic(*ty))
                    {
                        fixed.extend(return_types);
                        return SelectArgumentValues {
                            fixed,
                            tail: Some(TypePackTail::Variadic(self.primitives().any)),
                        };
                    }
                    if return_types.is_empty() && self.input.mode == Mode::Nonstrict {
                        return SelectArgumentValues {
                            fixed,
                            tail: Some(TypePackTail::Variadic(self.primitives().any)),
                        };
                    }
                    fixed.extend(return_types);
                    return SelectArgumentValues { fixed, tail: None };
                }
                if let Some(arg_tail) = arg_tail {
                    let normalized = self.arena.normalize_pack(arg_tail);
                    fixed.extend(normalized.types);
                    return SelectArgumentValues {
                        fixed,
                        tail: normalized.tail,
                    };
                }
                return SelectArgumentValues {
                    fixed,
                    tail: Some(TypePackTail::Variadic(self.primitives().any)),
                };
            }
            if let Some(arg_ty) = arg_types.get(arg_type_index).copied() {
                fixed.push(arg_ty);
            }
        }
        SelectArgumentValues { fixed, tail: None }
    }

    fn fixed_return_types_from_call_argument(
        &mut self,
        scope: ScopeId,
        value: &Expr,
    ) -> Option<Vec<TypeId>> {
        let Expr::Call {
            location,
            func,
            type_arguments,
            ..
        } = ungroup_expr(value)
        else {
            return None;
        };
        let callee = self.dfg_type_for_expr(func);
        let expected_callee =
            self.resolved_expected_callee(scope, func, callee, type_arguments, *location);
        (!self.function_is_generic(expected_callee))
            .then(|| self.function_fixed_return_types(expected_callee))
            .flatten()
    }

    fn pad_select_returns(
        &self,
        mut returns: Vec<TypeId>,
        tail: &Option<TypePackTail>,
        target_count: usize,
    ) -> Vec<TypeId> {
        while returns.len() < target_count {
            let fallback = match tail {
                Some(TypePackTail::Variadic(ty)) => *ty,
                Some(TypePackTail::Error) => self.primitives().error,
                _ => self.primitives().nil,
            };
            returns.push(fallback);
        }
        returns.truncate(target_count);
        returns
    }

    pub(crate) fn preserved_call_return_pack(
        &mut self,
        scope: ScopeId,
        value: &Expr,
    ) -> Option<TypePackId> {
        if let Some(pack) = self.assert_never_return_pack(scope, value) {
            return Some(pack);
        }
        if self.call_fixed_return_count(scope, value).is_some() {
            return None;
        }
        if let Some(pack) = self.call_variadic_return_pack(scope, value) {
            self.expr_type(scope, value);
            return Some(pack);
        }
        self.free_call_result_pack(scope, value, "call-return-results")
    }

    fn assert_never_return_pack(&mut self, scope: ScopeId, value: &Expr) -> Option<TypePackId> {
        let Expr::Call {
            func,
            args,
            is_self: false,
            ..
        } = ungroup_expr(value)
        else {
            return None;
        };
        if !matches!(func.as_ref(), Expr::Global { name, .. } if name.as_str() == "assert") {
            return None;
        }
        let first_arg = args.first()?;
        let first_arg_ty = self.expr_type(scope, first_arg);
        let result = self.assert_call_result_type(first_arg, first_arg_ty);
        if self.arena.follow(result) != self.primitives().never {
            return None;
        }
        self.expr_type(scope, value);
        let never = self.primitives().never;
        let tail = self.arena.alloc_pack(TypePackKind::Variadic { ty: never });
        Some(self.pack_with_tail(vec![never], Some(tail)))
    }

    fn assert_call_result_type(&mut self, arg: &Expr, arg_ty: TypeId) -> TypeId {
        let result = self.truthy_part(arg_ty);
        if self.assert_property_free_recovers_to_any(arg, result) {
            self.primitives().any
        } else {
            result
        }
    }

    fn assert_property_free_recovers_to_any(&self, arg: &Expr, result: TypeId) -> bool {
        let is_property_access = matches!(
            ungroup_expr(arg),
            Expr::IndexName { .. } | Expr::IndexExpr { .. }
        );
        is_property_access
            && matches!(
                self.arena.get(self.arena.follow(result)),
                TypeKind::Free(variable)
                    if variable.lower_bound.is_none() && variable.upper_bound.is_none()
            )
    }

    fn free_call_result_pack(
        &mut self,
        scope: ScopeId,
        value: &Expr,
        name: &str,
    ) -> Option<TypePackId> {
        let value = ungroup_expr(value);
        let Expr::Call { func, .. } = value else {
            return None;
        };
        if !self.expr_is_unannotated_function_parameter_path(func) {
            return None;
        }
        let callee = self.expr_type(scope, func);
        let expected_callee = self.callable_type(scope, func, callee);
        if !matches!(
            self.arena.get(self.arena.follow(expected_callee)),
            TypeKind::Free(_)
        ) {
            return None;
        }
        let returns = self.arena.alloc_pack(TypePackKind::Free {
            level: TypeLevel(0),
            name: Some(name.to_owned()),
        });
        self.calls
            .call_result_packs
            .insert(value.syntax_id(), returns);
        self.expr_type(scope, value);
        self.calls.call_result_packs.remove(&value.syntax_id());
        Some(returns)
    }

    fn nonstrict_checked_argument_rules_apply(&self, callee: TypeId) -> bool {
        if self.input.mode != Mode::Nonstrict {
            return false;
        }
        matches!(
            self.arena.get(self.arena.follow(callee)),
            TypeKind::Function(function) if function.is_checked
        )
    }
    pub(crate) fn table_insert_argument_types(
        &mut self,
        scope: ScopeId,
        args: &[Expr],
    ) -> (Vec<TypeId>, Option<TypePackId>) {
        let Some(table_arg) = args.first() else {
            return (Vec::new(), None);
        };
        let table_ty = self.expr_type(scope, table_arg);
        let mut arg_types = vec![table_ty];
        let mut arg_tail = None;
        match args {
            [_, value] => {
                self.push_table_insert_value_argument(
                    scope,
                    table_ty,
                    value,
                    &mut arg_types,
                    &mut arg_tail,
                );
            }
            [_, position, value] => {
                let number = self.primitives().number;
                let position_ty = self.expr_type_with_expected(scope, position, Some(number));
                arg_types.push(
                    if self.expected_accepts_without_subtype(position_ty, number) {
                        number
                    } else {
                        position_ty
                    },
                );
                self.push_table_insert_value_argument(
                    scope,
                    table_ty,
                    value,
                    &mut arg_types,
                    &mut arg_tail,
                );
            }
            [_, rest @ ..] => {
                for (index, arg) in rest.iter().enumerate() {
                    if index + 1 == rest.len()
                        && let Some(expansion) = self.final_call_argument_expansion(scope, arg)
                    {
                        if let FinalCallArgumentExpansion::Tail(tail) = expansion {
                            arg_tail = Some(tail);
                        }
                        continue;
                    }
                    arg_types.push(self.expr_type(scope, arg));
                }
            }
            [] => {}
        }
        (arg_types, arg_tail)
    }
    fn push_table_insert_value_argument(
        &mut self,
        scope: ScopeId,
        table_ty: TypeId,
        value: &Expr,
        arg_types: &mut Vec<TypeId>,
        arg_tail: &mut Option<TypePackId>,
    ) {
        if let Some(expansion) = self.final_call_argument_expansion(scope, value) {
            if let FinalCallArgumentExpansion::Tail(tail) = expansion {
                *arg_tail = Some(tail);
            }
            return;
        }

        let expected = self.table_insert_value_expected(table_ty);
        let actual = self.expr_type_with_expected(
            scope,
            value,
            self.table_insert_generation_expected(table_ty, expected),
        );
        arg_types.push(self.table_insert_argument_value_type(table_ty, actual, expected));
    }
    fn table_insert_generation_expected(
        &self,
        table: TypeId,
        expected: Option<TypeId>,
    ) -> Option<TypeId> {
        if self.table_insert_preserves_actual_value_type(table) {
            None
        } else {
            expected
        }
    }
    fn table_insert_argument_value_type(
        &self,
        table: TypeId,
        actual: TypeId,
        expected: Option<TypeId>,
    ) -> TypeId {
        if self.table_insert_preserves_actual_value_type(table) {
            actual
        } else {
            expected.unwrap_or(actual)
        }
    }
    fn table_insert_preserves_actual_value_type(&self, table: TypeId) -> bool {
        match self.arena.get(self.arena.follow(table)) {
            TypeKind::Table(table) => {
                matches!(table.state, TableState::Free | TableState::Unsealed)
            }
            TypeKind::Metatable { table, .. } => {
                self.table_insert_preserves_actual_value_type(*table)
            }
            TypeKind::Union(options) => options
                .iter()
                .any(|option| self.table_insert_preserves_actual_value_type(*option)),
            _ => false,
        }
    }
    fn table_insert_value_expected(&self, table: TypeId) -> Option<TypeId> {
        match self.arena.get(self.arena.follow(table)) {
            TypeKind::Table(table) => table.indexer.as_ref().and_then(|indexer| {
                Subtyper::new(self.arena)
                    .is_subtype(self.primitives().number, indexer.key)
                    .is_ok()
                    .then_some(indexer.value)
            }),
            TypeKind::Metatable { table, .. } => self.table_insert_value_expected(*table),
            TypeKind::Union(options) => {
                let mut expected = None;
                for option in options {
                    let option_expected = self.table_insert_value_expected(*option)?;
                    if let Some(existing) = expected {
                        if self.arena.follow(existing) != self.arena.follow(option_expected) {
                            return None;
                        }
                    } else {
                        expected = Some(option_expected);
                    }
                }
                expected
            }
            _ => None,
        }
    }
    pub(crate) fn instantiate_expected_call_callee(
        &mut self,
        expected_callee: TypeId,
    ) -> (TypeId, bool) {
        let mut followed = self.arena.follow(expected_callee);
        let TypeKind::Function(function) = self.arena.get(followed).clone() else {
            return (expected_callee, false);
        };
        if function.generics.is_empty()
            && function.generic_packs.is_empty()
            && function_signature_has_callback_free_correlation(self.arena, followed)
        {
            // The solver generalizes these signatures at the call site. Do the
            // same before providing contextual callback types so callback
            // returns bind a per-call pack, not the shared function surface.
            followed = crate::generalize::generalize_function_frees(self.arena, followed);
        }
        let TypeKind::Function(function) = self.arena.get(followed) else {
            return (expected_callee, false);
        };
        if function.generics.is_empty() && function.generic_packs.is_empty() {
            return (expected_callee, false);
        }
        (
            Instantiator::new(self.arena, TypeLevel(0)).instantiate_type(followed),
            true,
        )
    }

    fn function_has_self(&self, ty: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(ty)),
            TypeKind::Function(function) if function.has_self
        )
    }

    pub(crate) fn explicit_type_instantiation(
        &mut self,
        scope: ScopeId,
        callee: TypeId,
        type_arguments: &[TypeParameter],
        location: Option<Location>,
    ) -> TypeId {
        let followed = self.arena.follow(callee);
        match self.arena.get(followed).clone() {
            TypeKind::Function(function) => {
                self.explicit_function_instantiation(scope, function, type_arguments, location)
            }
            TypeKind::Intersection(_) => {
                self.report_explicit_type_instantiation_not_function(location);
                self.primitives().error
            }
            TypeKind::Metatable { .. } => {
                self.report_explicit_type_instantiation_not_function(location);
                self.primitives().error
            }
            TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Error
            | TypeKind::Free(_)
            | TypeKind::Blocked(_) => callee,
            _ => {
                self.report_explicit_type_instantiation_not_function(location);
                self.primitives().error
            }
        }
    }

    fn explicit_function_instantiation(
        &mut self,
        scope: ScopeId,
        function: FunctionType,
        type_arguments: &[TypeParameter],
        location: Option<Location>,
    ) -> TypeId {
        if function.generics.is_empty() && function.generic_packs.is_empty() {
            self.report_explicit_type_instantiation_parameter_count(
                &function,
                type_arguments,
                location,
            );
            return self.primitives().error;
        }

        let mut type_bindings = Vec::new();
        let mut pack_bindings = Vec::new();
        let mut parameter_index = 0;

        for generic in &function.generics {
            let Some(argument) = type_arguments.get(parameter_index) else {
                break;
            };
            match argument {
                TypeParameter::Type(ty) => {
                    let replacement = self.lower_type(scope, ty);
                    type_bindings.push((generic.clone(), replacement));
                    parameter_index += 1;
                }
                TypeParameter::Pack(_) => {
                    self.report_explicit_type_instantiation_parameter_count(
                        &function,
                        type_arguments,
                        location,
                    );
                    return self.primitives().error;
                }
            }
        }

        for (pack_index, generic_pack) in function.generic_packs.iter().enumerate() {
            let Some(argument) = type_arguments.get(parameter_index) else {
                break;
            };
            match argument {
                TypeParameter::Pack(pack) => {
                    let replacement = self.lower_type_pack(scope, pack);
                    pack_bindings.push((generic_pack.clone(), replacement));
                    parameter_index += 1;
                }
                TypeParameter::Type(_) if pack_index == 0 => {
                    let mut types = Vec::new();
                    while let Some(TypeParameter::Type(ty)) = type_arguments.get(parameter_index) {
                        types.push(self.lower_type(scope, ty));
                        parameter_index += 1;
                    }
                    if function.generic_packs.len() != 1 {
                        self.report_explicit_type_instantiation_parameter_count(
                            &function,
                            type_arguments,
                            location,
                        );
                        return self.primitives().error;
                    }
                    let replacement = self
                        .arena
                        .alloc_pack(TypePackKind::List { types, tail: None });
                    pack_bindings.push((generic_pack.clone(), replacement));
                }
                TypeParameter::Type(_) => {
                    self.report_explicit_type_instantiation_parameter_count(
                        &function,
                        type_arguments,
                        location,
                    );
                    return self.primitives().error;
                }
            }
        }

        if parameter_index < type_arguments.len() {
            if type_arguments[parameter_index..]
                .iter()
                .all(|parameter| matches!(parameter, TypeParameter::Pack(_)))
            {
                return self.primitives().error;
            }
            self.report_explicit_type_instantiation_parameter_count(
                &function,
                type_arguments,
                location,
            );
            return self.primitives().error;
        }

        let mut instantiator = Instantiator::new(self.arena, TypeLevel(0));
        for (generic, replacement) in &type_bindings {
            instantiator.bind_generic(generic, *replacement);
        }
        for (generic, replacement) in &pack_bindings {
            instantiator.bind_generic_pack(generic, *replacement);
        }
        let arguments = instantiator.instantiate_pack(function.arguments);
        let returns = instantiator.instantiate_pack(function.returns);
        // Substituting the explicit type arguments can turn a deferred type
        // function (`index<ST, T>`) into a now-concrete one (`index<ST,
        // "Member1">`); reduce those so the call's result is the resolved type.
        let returns = self.reduce_pack_type_function_instances(returns);
        self.arena.alloc(TypeKind::Function(FunctionType {
            generics: Vec::new(),
            generic_packs: Vec::new(),
            argument_names: function.argument_names,
            has_self: function.has_self,
            is_checked: function.is_checked,
            arguments,
            returns,
        }))
    }

    /// Reduces any top-level type-function instances in a pack's fixed types,
    /// used after explicit generic instantiation makes their arguments concrete.
    fn reduce_pack_type_function_instances(&mut self, pack: TypePackId) -> TypePackId {
        let pack = self.arena.follow_pack(pack);
        let TypePackKind::List { types, tail } = self.arena.get_pack(pack).clone() else {
            return pack;
        };
        let mut changed = false;
        let reduced = types
            .iter()
            .map(|&ty| {
                let (reduced, did) = self.reduce_alias_type_function(ty);
                changed |= did;
                reduced
            })
            .collect::<Vec<_>>();
        if !changed {
            return pack;
        }
        self.arena.alloc_pack(TypePackKind::List {
            types: reduced,
            tail,
        })
    }

    pub(crate) fn explicit_table_builtin_instantiation(
        &mut self,
        scope: ScopeId,
        method: &str,
        type_arguments: &[TypeParameter],
        location: Option<Location>,
    ) -> TypeId {
        let [TypeParameter::Type(value_ty)] = type_arguments else {
            let expected = FunctionType {
                generics: vec![crate::types::GenericType {
                    name: "V".to_owned(),
                    level: TypeLevel(0),
                }],
                generic_packs: Vec::new(),
                argument_names: Vec::new(),
                has_self: false,
                is_checked: false,
                arguments: self.arena.empty_pack(),
                returns: self.arena.empty_pack(),
            };
            self.report_explicit_type_instantiation_parameter_count(
                &expected,
                type_arguments,
                location,
            );
            return self.primitives().error;
        };
        let value_ty = self.lower_type(scope, value_ty);
        match method {
            "create" => self.table_create_type(value_ty),
            "find" => self.table_find_type(value_ty),
            "unpack" => self.table_unpack_type(value_ty),
            _ => self.primitives().error,
        }
    }

    fn table_create_type(&mut self, value_ty: TypeId) -> TypeId {
        let optional_value = self.optional_type(value_ty);
        let arguments = self.pack(vec![self.primitives().number, optional_value]);
        let array = self.array_table_type(value_ty);
        let returns = self.pack(vec![array]);
        self.arena
            .alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
    }

    fn table_find_type(&mut self, value_ty: TypeId) -> TypeId {
        let array = self.array_table_type(value_ty);
        let optional_number = self.optional_number_type();
        let arguments = self.pack(vec![array, value_ty, optional_number]);
        let optional_number = self.optional_number_type();
        let returns = self.pack(vec![optional_number]);
        self.arena
            .alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
    }

    fn table_unpack_type(&mut self, value_ty: TypeId) -> TypeId {
        let array = self.array_table_type(value_ty);
        let optional_start = self.optional_number_type();
        let optional_end = self.optional_number_type();
        let arguments = self.pack(vec![array, optional_start, optional_end]);
        let returns = self
            .arena
            .alloc_pack(TypePackKind::Variadic { ty: value_ty });
        self.arena
            .alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
    }

    fn array_table_type(&mut self, value_ty: TypeId) -> TypeId {
        let mut table = TableType::new(TableState::Sealed);
        table.indexer = Some(TableIndexer {
            key: self.primitives().number,
            value: value_ty,
            read_only: false,
        });
        self.arena.alloc(TypeKind::Table(table))
    }

    fn report_explicit_type_instantiation_not_function(&mut self, location: Option<Location>) {
        let diagnostic = Diagnostic::error(
            DiagnosticCategory::Generic,
            DiagnosticLocation::from_opt(location),
        )
        .with_typed(Payload::ExplicitTypeInstantiationNotFunction);
        self.generated.diagnostics.push(diagnostic);
    }

    fn report_explicit_type_instantiation_parameter_count(
        &mut self,
        function: &FunctionType,
        type_arguments: &[TypeParameter],
        location: Option<Location>,
    ) {
        let type_argument_count = type_arguments
            .iter()
            .filter(|parameter| matches!(parameter, TypeParameter::Type(_)))
            .count();
        let pack_argument_count = type_arguments.len() - type_argument_count;
        self.generated.diagnostics.push(
            Diagnostic::error(
                DiagnosticCategory::Generic,
                DiagnosticLocation::from_opt(location),
            )
            .with_typed(Payload::ExplicitTypeInstantiationParameterCount {
                expected_types: function.generics.len(),
                expected_packs: function.generic_packs.len(),
                actual_types: type_argument_count,
                actual_packs: pack_argument_count,
            }),
        );
    }

    fn bind_call_expected_parameter(&mut self, actual: TypeId, expected: TypeId) {
        let expected = self.arena.follow(expected);
        let actual = self.arena.follow(actual);
        if actual == expected {
            return;
        }
        match (
            self.arena.get(actual).clone(),
            self.arena.get(expected).clone(),
        ) {
            (_, TypeKind::Free(_)) => {
                if matches!(
                    self.arena.get(actual),
                    TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Free(_)
                ) {
                    return;
                }
                self.bind_free_to(expected, actual);
            }
            (TypeKind::Table(actual_table), TypeKind::Table(expected_table)) => {
                if actual_table.instantiated_type_params.len()
                    == expected_table.instantiated_type_params.len()
                {
                    for (actual, expected) in actual_table
                        .instantiated_type_params
                        .into_iter()
                        .zip(expected_table.instantiated_type_params)
                    {
                        self.bind_call_expected_parameter(actual, expected);
                    }
                }
                for (name, expected_property) in expected_table.properties {
                    if let Some(actual_property) = actual_table.properties.get(&name) {
                        self.bind_call_expected_parameter(actual_property.ty, expected_property.ty);
                    }
                }
                if let (Some(actual_indexer), Some(expected_indexer)) =
                    (actual_table.indexer, expected_table.indexer)
                {
                    self.bind_call_expected_parameter(actual_indexer.key, expected_indexer.key);
                    self.bind_call_expected_parameter(actual_indexer.value, expected_indexer.value);
                }
            }
            _ => {}
        }
    }

    fn reduce_call_argument_expected_type(
        &mut self,
        scope: ScopeId,
        expected: TypeId,
        location: Option<DiagnosticLocation>,
    ) -> TypeId {
        let expected = self.arena.follow(expected);
        let TypeKind::TypeFunctionInstance { name, arguments } = self.arena.get(expected).clone()
        else {
            return expected;
        };
        let Some((binding_scope, binding)) = self.input.scopes.lookup_type_with_scope(scope, &name)
        else {
            return expected;
        };
        if binding.kind != TypeBindingKind::TypeFunction {
            return expected;
        }
        let Some(func) = binding.type_function.clone() else {
            return expected;
        };
        match self.reduce_user_type_function_with_arguments(
            binding_scope,
            &name,
            &func,
            arguments,
            location,
        ) {
            TypeFunctionEvaluation::Reduced(reduced) => reduced,
            TypeFunctionEvaluation::Uninhabited => {
                self.report_uninhabited_type_function_diagnostic(expected, location);
                expected
            }
            TypeFunctionEvaluation::RuntimeError | TypeFunctionEvaluation::Deferred => expected,
        }
    }

    fn reduce_call_callee_parameter_type_functions(
        &mut self,
        scope: ScopeId,
        callee: TypeId,
    ) -> Option<TypeId> {
        let followed_callee = self.arena.follow(callee);
        let TypeKind::Function(mut function) = self.arena.get(followed_callee).clone() else {
            return None;
        };
        let TypePackKind::List { types, tail } = self
            .arena
            .get_pack(self.arena.follow_pack(function.arguments))
            .clone()
        else {
            return None;
        };
        let mut changed = false;
        let types = types
            .into_iter()
            .map(|ty| {
                let reduced = self.reduce_call_argument_expected_type(scope, ty, None);
                changed |= self.arena.follow(reduced) != self.arena.follow(ty);
                reduced
            })
            .collect::<Vec<_>>();
        if !changed {
            return None;
        }
        function.arguments = self.arena.alloc_pack(TypePackKind::List { types, tail });
        Some(self.arena.alloc(TypeKind::Function(function)))
    }

    fn bind_scalar_call_argument_expected_type_if_needed(
        &mut self,
        arg: &Expr,
        actual: TypeId,
        expected: TypeId,
    ) {
        if self
            .function_parameter_expected_argument_type(arg, expected)
            .is_some()
        {
            return;
        }
        let actual = self.arena.follow(actual);
        if !matches!(self.arena.get(actual), TypeKind::Free(_)) {
            return;
        }
        let expected = self.arena.follow(expected);
        if !matches!(
            self.arena.get(expected),
            TypeKind::Primitive(_) | TypeKind::Singleton(_)
        ) {
            return;
        }
        self.bind_free_to(actual, expected);
    }

    fn push_concrete_call_argument_subtype_if_needed(
        &mut self,
        arg: &Expr,
        actual: TypeId,
        expected: TypeId,
    ) {
        if !self.expected_accepts_without_subtype(actual, expected)
            || !self.concrete_call_argument_has_non_suppressing_table_mismatch(actual, expected)
        {
            return;
        }
        self.generated
            .constraints
            .push(Constraint::expected_subtype(
                actual,
                expected,
                arg.location().map(DiagnosticLocation::from),
                false,
            ));
    }
    fn concrete_call_argument_has_non_suppressing_table_mismatch(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> bool {
        if !matches!(
            (
                self.arena.get(self.arena.follow(actual)),
                self.arena.get(self.arena.follow(expected)),
            ),
            (TypeKind::Table(_), TypeKind::Table(_))
        ) {
            return false;
        }
        let Err(error) = Subtyper::new(self.arena).is_subtype(actual, expected) else {
            return false;
        };
        if !matches!(
            error.kind,
            SubtypeErrorKind::Mismatch | SubtypeErrorKind::PropertyVariance
        ) {
            return false;
        }
        let suppression = Subtyper::new(self.arena).suppression(actual, expected);
        !suppression.fully_suppressing
            && self.subtype_target_is_concrete(error.sub)
            && self.subtype_target_is_concrete(error.sup)
    }
    fn subtype_target_is_concrete(&self, target: SubtypeTarget) -> bool {
        match target {
            SubtypeTarget::Type(ty) => matches!(
                self.arena.get(self.arena.follow(ty)),
                TypeKind::Primitive(_)
                    | TypeKind::Singleton(_)
                    | TypeKind::Function(_)
                    | TypeKind::Table(_)
                    | TypeKind::Metatable { .. }
                    | TypeKind::Extern { .. }
                    | TypeKind::Never
            ),
            SubtypeTarget::Pack(pack) => matches!(
                self.arena.get_pack(self.arena.follow_pack(pack)),
                TypePackKind::List { .. } | TypePackKind::Variadic { .. }
            ),
        }
    }
    pub(crate) fn call_fixed_return_count(
        &mut self,
        scope: ScopeId,
        value: &Expr,
    ) -> Option<usize> {
        let value = ungroup_expr(value);
        let Expr::Call {
            location,
            func,
            type_arguments,
            ..
        } = value
        else {
            return None;
        };
        let callee = self.expr_type(scope, func);
        let expected_callee =
            self.resolved_expected_callee(scope, func, callee, type_arguments, *location);
        self.function_fixed_return_count(expected_callee)
    }
    fn call_variadic_return_pack(&mut self, scope: ScopeId, value: &Expr) -> Option<TypePackId> {
        let value = ungroup_expr(value);
        let Expr::Call { func, .. } = value else {
            return None;
        };
        let callee = self.expr_type(scope, func);
        let expected_callee = self.callable_type(scope, func, callee);
        let expected_callee = self.arena.follow(expected_callee);
        let TypeKind::Function(function) = self.arena.get(expected_callee) else {
            return None;
        };
        let returns = self.arena.normalize_pack(function.returns);
        returns.tail.is_some().then_some(function.returns)
    }
    pub(crate) fn ipairs_iterator_type(&mut self, table_ty: TypeId, value_ty: TypeId) -> TypeId {
        let number = self.primitives().number;
        let maybe_number = self.union_type(vec![self.primitives().nil, number]);
        let arguments = self.pack(vec![table_ty, number]);
        let returns = self.pack(vec![maybe_number, value_ty]);
        self.arena
            .alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
    }
    pub(crate) fn pairs_iterator_type(
        &mut self,
        table_ty: TypeId,
        key_ty: TypeId,
        value_ty: TypeId,
    ) -> TypeId {
        let maybe_key = self.optional_type(key_ty);
        let arguments = self.pack(vec![table_ty, maybe_key]);
        let returns = self.pack(vec![maybe_key, value_ty]);
        self.arena
            .alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
    }
    fn dynamic_pairs_call_iteration_types(&self, ty: TypeId) -> Option<(TypeId, TypeId)> {
        matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Any)
            .then(|| (self.primitives().any, self.primitives().any))
    }
    pub(crate) fn function_argument_types(&self, callee: TypeId) -> Vec<TypeId> {
        let callee = self.arena.follow(callee);
        match self.arena.get(callee) {
            TypeKind::Function(function) => self.arena.normalize_pack(function.arguments).types,
            _ => Vec::new(),
        }
    }
    pub(crate) fn bind_missing_free_call_arguments_to_nil(
        &mut self,
        callee: TypeId,
        supplied_count: usize,
    ) {
        let expected_parameters =
            ExpectedCallParameterPack::from_callee(self.arena, callee, ReceiverParameter::Explicit);
        for parameter in expected_parameters
            .fixed_parameters()
            .iter()
            .copied()
            .skip(supplied_count)
        {
            if matches!(
                self.arena.get(self.arena.follow(parameter)),
                TypeKind::Free(_)
            ) {
                self.bind_free_to(parameter, self.primitives().nil);
            }
        }
    }
    pub(crate) fn constrain_missing_free_call_arguments_to_nil(
        &mut self,
        callee: TypeId,
        supplied_count: usize,
        location: Option<DiagnosticLocation>,
    ) {
        let expected_parameters =
            ExpectedCallParameterPack::from_callee(self.arena, callee, ReceiverParameter::Explicit);
        for parameter in expected_parameters
            .fixed_parameters()
            .iter()
            .copied()
            .skip(supplied_count)
        {
            if matches!(
                self.arena.get(self.arena.follow(parameter)),
                TypeKind::Free(_)
            ) {
                self.generated
                    .constraints
                    .push(Constraint::unify_default_location(
                        parameter,
                        self.primitives().nil,
                        location,
                    ));
            }
        }
    }
    pub(crate) fn report_too_few_call_arguments(
        &mut self,
        callee: TypeId,
        supplied: CallArgumentSupply,
        location: Option<DiagnosticLocation>,
    ) -> bool {
        let Some(supplied_count) = supplied.definite_count() else {
            return false;
        };
        let (required_count, missing_foreign_generic_tail) =
            self.function_required_argument_details(callee, supplied_count);
        if supplied_count >= required_count {
            return false;
        }
        if self.missing_required_parameters_are_free(callee, supplied_count, required_count) {
            return false;
        }

        self.generated.diagnostics.push(
            Diagnostic::error(
                if missing_foreign_generic_tail {
                    DiagnosticCategory::TypePack
                } else {
                    DiagnosticCategory::Call
                },
                location.unwrap_or_else(DiagnosticLocation::missing),
            )
            .with_typed(Payload::ArityMismatch {
                counts: Some(crate::diagnostics::ArityCounts {
                    expected: required_count,
                    actual: supplied_count,
                }),
                subtype: crate::diagnostics::SubtypeContext::default(),
            }),
        );
        true
    }
    fn missing_required_parameters_are_free(
        &self,
        callee: TypeId,
        supplied_count: usize,
        required_count: usize,
    ) -> bool {
        let missing_count = required_count.saturating_sub(supplied_count);
        if missing_count == 0 {
            return false;
        }
        let expected_parameters =
            ExpectedCallParameterPack::from_callee(self.arena, callee, ReceiverParameter::Explicit);
        let missing = expected_parameters
            .fixed_parameters()
            .iter()
            .copied()
            .skip(supplied_count)
            .take(missing_count)
            .collect::<Vec<_>>();
        !missing.is_empty()
            && missing.iter().all(|parameter| {
                matches!(
                    self.arena.get(self.arena.follow(*parameter)),
                    TypeKind::Free(_)
                )
            })
    }
    pub(crate) fn function_required_argument_count(&self, callee: TypeId) -> usize {
        self.function_required_argument_details(callee, usize::MAX)
            .0
    }

    pub(crate) fn report_generic_pack_call_argument_mismatch(
        &mut self,
        callee: TypeId,
        arg_types: &[TypeId],
        arg_tail: Option<TypePackId>,
        argument_locations: &[Option<DiagnosticLocation>],
        location: Option<DiagnosticLocation>,
    ) -> bool {
        use crate::generation::generic_pack_call::GenericPackCallMismatch;

        let Some(mismatch) =
            crate::generation::generic_pack_call::generic_pack_call_argument_mismatch(
                self.arena, callee, arg_types, arg_tail,
            )
        else {
            return false;
        };

        // A scalar argument that fails its expected type is an ordinary type
        // mismatch reported at the offending argument's span; only an
        // argument-count failure is a type-pack mismatch at the call.
        let (category, type_mismatch, span) = match mismatch {
            GenericPackCallMismatch::Arity => (DiagnosticCategory::TypePack, false, location),
            GenericPackCallMismatch::ScalarType { argument_index } => (
                DiagnosticCategory::TypeMismatch,
                true,
                argument_locations
                    .get(argument_index)
                    .copied()
                    .flatten()
                    .or(location),
            ),
        };

        self.generated.diagnostics.push(
            Diagnostic::error(category, span.unwrap_or_else(DiagnosticLocation::missing))
                .with_typed(Payload::GenericPackCallArgumentMismatch { type_mismatch }),
        );
        true
    }

    fn function_required_argument_details(
        &self,
        callee: TypeId,
        supplied_count: usize,
    ) -> (usize, bool) {
        let callee = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(callee) else {
            return (0, false);
        };
        let arguments = self.arena.normalize_pack(function.arguments);
        let fixed_required = arguments
            .types
            .iter()
            .rposition(|ty| !self.type_accepts_nil(*ty, &mut BTreeSet::new()))
            .map_or(0, |index| index + 1);
        let tail_required = match arguments.tail {
            Some(TypePackTail::Generic(ref pack))
                if !function.generic_packs.iter().any(|owned| owned == pack) =>
            {
                arguments.types.len() + 1
            }
            _ => 0,
        };
        let required = fixed_required.max(tail_required);
        let missing_foreign_generic_tail =
            tail_required > fixed_required && supplied_count >= fixed_required;
        (required, missing_foreign_generic_tail)
    }
    /// Returns true when the callee carries quantified type-parameter
    /// generics whose return position references them. Pack-parameter
    /// generics (`<A...>(...) -> A...`) keep the known-return
    /// shortcut because their variadic-return shape is preserved
    /// through `function_result_type`; only the type-parameter case
    /// needs the constraint-solver instantiation hand-off.
    pub(crate) fn placeholder_free_type(&mut self, name: &str) -> TypeId {
        self.arena.alloc(TypeKind::Free(crate::types::TypeVariable {
            level: TypeLevel(0),
            name: Some(name.to_string()),
            lower_bound: None,
            upper_bound: None,
        }))
    }
    pub(crate) fn function_is_generic(&self, callee: TypeId) -> bool {
        let callee = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(callee) else {
            return false;
        };
        // Check if quantified generics appear in the return surface — only then
        // does the known-return shortcut produce a stale type. Direct
        // generic-pack tails are handled by the dedicated pack path; nested
        // returned functions still need the constraint-solver instantiation
        // hand-off.
        (!function.generics.is_empty()
            && self.pack_references_generic_type(function.returns, &function.generics))
            || (!function.generic_packs.is_empty()
                && self.pack_references_generic_pack(function.returns, &function.generic_packs))
            || self.pack_references_unbound_free(function.returns)
    }

    fn function_returns_own_generic_pack(&self, callee: TypeId) -> bool {
        let callee = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(callee) else {
            return false;
        };
        if function.generic_packs.is_empty() {
            return false;
        }
        let returns = self.arena.normalize_pack(function.returns);
        matches!(
            returns.tail,
            Some(TypePackTail::Generic(pack))
                if function.generic_packs.iter().any(|generic| generic == &pack)
        )
    }

    fn type_cannot_use_expected_guidance(&self, ty: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(ty)),
            TypeKind::Generic(_)
                | TypeKind::Free(_)
                | TypeKind::Function(_)
                | TypeKind::TypeFunctionInstance { .. }
        )
    }

    fn pack_references_generic_type(
        &self,
        pack: TypePackId,
        generics: &[crate::types::GenericType],
    ) -> bool {
        self.pack_references_generic_type_inner(
            pack,
            generics,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        )
    }

    fn pack_references_generic_type_inner(
        &self,
        pack: TypePackId,
        generics: &[crate::types::GenericType],
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack).clone() {
            TypePackKind::List { types, tail } => {
                types
                    .iter()
                    .any(|ty| self.type_references_generic(*ty, generics, seen_types, seen_packs))
                    || tail.is_some_and(|tail| {
                        self.pack_references_generic_type_inner(
                            tail, generics, seen_types, seen_packs,
                        )
                    })
            }
            TypePackKind::Variadic { ty } => {
                self.type_references_generic(ty, generics, seen_types, seen_packs)
            }
            TypePackKind::Bound(bound) => {
                self.pack_references_generic_type_inner(bound, generics, seen_types, seen_packs)
            }
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }

    fn type_references_generic(
        &self,
        ty: TypeId,
        generics: &[crate::types::GenericType],
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return false;
        }
        match self.arena.get(ty).clone() {
            TypeKind::Generic(generic) => generics.iter().any(|g| g.name == generic.name),
            TypeKind::Function(function) => {
                self.pack_references_generic_type_inner(
                    function.arguments,
                    generics,
                    seen_types,
                    seen_packs,
                ) || self.pack_references_generic_type_inner(
                    function.returns,
                    generics,
                    seen_types,
                    seen_packs,
                )
            }
            TypeKind::Table(table) => {
                table
                    .instantiated_type_params
                    .iter()
                    .any(|ty| self.type_references_generic(*ty, generics, seen_types, seen_packs))
                    || table.properties.values().any(|property| {
                        self.type_references_generic(property.ty, generics, seen_types, seen_packs)
                    })
                    || table.indexer.is_some_and(|indexer| {
                        self.type_references_generic(indexer.key, generics, seen_types, seen_packs)
                            || self.type_references_generic(
                                indexer.value,
                                generics,
                                seen_types,
                                seen_packs,
                            )
                    })
            }
            TypeKind::Extern { properties, .. } => properties.values().any(|property| {
                self.type_references_generic(property.ty, generics, seen_types, seen_packs)
            }),
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_references_generic(table, generics, seen_types, seen_packs)
                    || self.type_references_generic(metatable, generics, seen_types, seen_packs)
            }
            TypeKind::TypeFunctionInstance { arguments, .. } => arguments.iter().any(|argument| {
                self.type_references_generic(*argument, generics, seen_types, seen_packs)
            }),
            TypeKind::Union(options) | TypeKind::Intersection(options) => {
                options.iter().any(|option| {
                    self.type_references_generic(*option, generics, seen_types, seen_packs)
                })
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.type_references_generic(inner, generics, seen_types, seen_packs)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }

    fn pack_references_generic_pack(
        &self,
        pack: TypePackId,
        generics: &[crate::types::GenericTypePack],
    ) -> bool {
        self.pack_references_generic_pack_inner(
            pack,
            generics,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        )
    }

    fn pack_references_generic_pack_inner(
        &self,
        pack: TypePackId,
        generics: &[crate::types::GenericTypePack],
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack).clone() {
            TypePackKind::List { types, tail } => {
                types.iter().any(|ty| {
                    self.type_references_generic_pack(*ty, generics, seen_types, seen_packs)
                }) || tail.is_some_and(|tail| {
                    self.pack_references_generic_pack_inner(tail, generics, seen_types, seen_packs)
                })
            }
            TypePackKind::Variadic { ty } => {
                self.type_references_generic_pack(ty, generics, seen_types, seen_packs)
            }
            TypePackKind::Bound(bound) => {
                self.pack_references_generic_pack_inner(bound, generics, seen_types, seen_packs)
            }
            TypePackKind::Generic(pack) => generics.iter().any(|generic| generic == &pack),
            TypePackKind::Free { .. } | TypePackKind::Error => false,
        }
    }

    fn type_references_generic_pack(
        &self,
        ty: TypeId,
        generics: &[crate::types::GenericTypePack],
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return false;
        }
        match self.arena.get(ty).clone() {
            TypeKind::Function(function) => {
                self.pack_references_generic_pack_inner(
                    function.arguments,
                    generics,
                    seen_types,
                    seen_packs,
                ) || self.pack_references_generic_pack_inner(
                    function.returns,
                    generics,
                    seen_types,
                    seen_packs,
                )
            }
            TypeKind::Table(table) => {
                table.instantiated_type_pack_params.iter().any(|pack| {
                    self.pack_references_generic_pack_inner(*pack, generics, seen_types, seen_packs)
                }) || table.instantiated_type_params.iter().any(|ty| {
                    self.type_references_generic_pack(*ty, generics, seen_types, seen_packs)
                }) || table.properties.values().any(|property| {
                    self.type_references_generic_pack(property.ty, generics, seen_types, seen_packs)
                }) || table.indexer.is_some_and(|indexer| {
                    self.type_references_generic_pack(indexer.key, generics, seen_types, seen_packs)
                        || self.type_references_generic_pack(
                            indexer.value,
                            generics,
                            seen_types,
                            seen_packs,
                        )
                })
            }
            TypeKind::Extern { properties, .. } => properties.values().any(|property| {
                self.type_references_generic_pack(property.ty, generics, seen_types, seen_packs)
            }),
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_references_generic_pack(table, generics, seen_types, seen_packs)
                    || self
                        .type_references_generic_pack(metatable, generics, seen_types, seen_packs)
            }
            TypeKind::TypeFunctionInstance { arguments, .. } => arguments.iter().any(|argument| {
                self.type_references_generic_pack(*argument, generics, seen_types, seen_packs)
            }),
            TypeKind::Union(options) | TypeKind::Intersection(options) => {
                options.iter().any(|option| {
                    self.type_references_generic_pack(*option, generics, seen_types, seen_packs)
                })
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.type_references_generic_pack(inner, generics, seen_types, seen_packs)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Free(_)
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }

    fn pack_references_unbound_free(&self, pack: TypePackId) -> bool {
        self.pack_references_unbound_free_inner(pack, &mut BTreeSet::new(), &mut BTreeSet::new())
    }

    fn pack_references_unbound_free_inner(
        &self,
        pack: TypePackId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack).clone() {
            TypePackKind::List { types, tail } => {
                types
                    .iter()
                    .any(|ty| self.type_references_unbound_free(*ty, seen_types, seen_packs))
                    || tail.is_some_and(|tail| {
                        self.pack_references_unbound_free_inner(tail, seen_types, seen_packs)
                    })
            }
            TypePackKind::Variadic { ty } => {
                self.type_references_unbound_free(ty, seen_types, seen_packs)
            }
            TypePackKind::Bound(bound) => {
                self.pack_references_unbound_free_inner(bound, seen_types, seen_packs)
            }
            TypePackKind::Free { .. } => true,
            TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }

    fn type_references_unbound_free(
        &self,
        ty: TypeId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return false;
        }
        match self.arena.get(ty).clone() {
            TypeKind::Free(variable) => {
                variable.lower_bound.is_none() && variable.upper_bound.is_none()
            }
            TypeKind::Function(function) => {
                self.pack_references_unbound_free_inner(function.arguments, seen_types, seen_packs)
                    || self.pack_references_unbound_free_inner(
                        function.returns,
                        seen_types,
                        seen_packs,
                    )
            }
            TypeKind::Table(table) => {
                table
                    .instantiated_type_params
                    .iter()
                    .any(|ty| self.type_references_unbound_free(*ty, seen_types, seen_packs))
                    || table.properties.values().any(|property| {
                        self.type_references_unbound_free(property.ty, seen_types, seen_packs)
                    })
                    || table.indexer.is_some_and(|indexer| {
                        self.type_references_unbound_free(indexer.key, seen_types, seen_packs)
                            || self.type_references_unbound_free(
                                indexer.value,
                                seen_types,
                                seen_packs,
                            )
                    })
            }
            TypeKind::Extern { properties, .. } => properties.values().any(|property| {
                self.type_references_unbound_free(property.ty, seen_types, seen_packs)
            }),
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_references_unbound_free(table, seen_types, seen_packs)
                    || self.type_references_unbound_free(metatable, seen_types, seen_packs)
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => arguments.iter().any(|argument| {
                self.type_references_unbound_free(*argument, seen_types, seen_packs)
            }),
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.type_references_unbound_free(inner, seen_types, seen_packs)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }

    pub(crate) fn function_result_type(&self, callee: TypeId) -> Option<TypeId> {
        let callee = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(callee) else {
            return None;
        };
        let returns = self.arena.normalize_pack(function.returns);
        returns
            .types
            .first()
            .copied()
            .or_else(|| match returns.tail {
                Some(TypePackTail::Variadic(ty)) => Some(ty),
                Some(TypePackTail::Error) => Some(self.primitives().error),
                _ => None,
            })
    }
    pub(crate) fn function_fixed_return_count(&self, callee: TypeId) -> Option<usize> {
        let callee = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(callee) else {
            return None;
        };
        let returns = self.arena.normalize_pack(function.returns);
        returns.tail.is_none().then_some(returns.types.len())
    }
    pub(crate) fn call_fixed_return_count_from_dfg(
        &mut self,
        scope: ScopeId,
        value: &Expr,
    ) -> Option<usize> {
        let value = ungroup_expr(value);
        let Expr::Call { func, .. } = value else {
            return None;
        };
        let fallback = self.dfg_type_for_expr(func);
        let callee = self.callable_type(scope, func, fallback);
        self.function_fixed_return_count(callee)
    }
    pub(crate) fn function_fixed_return_types(&self, callee: TypeId) -> Option<Vec<TypeId>> {
        let callee = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(callee) else {
            return None;
        };
        let returns = self.arena.normalize_pack(function.returns);
        returns.tail.is_none().then_some(returns.types)
    }
    pub(crate) fn vararg_type_at(&self, index: usize) -> Option<TypeId> {
        let pack = self
            .function_frames
            .vararg_stack
            .last()
            .and_then(|pack| *pack)?;
        let normalized = self.arena.normalize_pack(pack);
        normalized
            .types
            .get(index)
            .copied()
            .or_else(|| match normalized.tail {
                Some(TypePackTail::Variadic(ty)) => Some(ty),
                Some(TypePackTail::Error) => Some(self.primitives().error),
                _ => None,
            })
    }
    pub(crate) fn inferred_return_pack(
        &mut self,
        returns: &[InferredReturnPath],
        seal_table_returns: bool,
    ) -> TypePackId {
        if let [only] = returns
            && let Some(pack) = only.pack
        {
            return pack;
        }

        // Nonstrict mode collapses divergent return arities to the shortest
        // return path — a value past the shortest `return` is not guaranteed —
        // so `return 3 … return 8, "x"` infers a single `number`, not
        // `(number, string?)`. Strict mode keeps the full (widest) arity.
        let arity = if self.input.mode == Mode::Nonstrict {
            returns
                .iter()
                .map(|path| path.fixed.len())
                .min()
                .unwrap_or(0)
        } else {
            returns
                .iter()
                .map(|path| path.fixed.len())
                .max()
                .unwrap_or(0)
        };
        let types = (0..arity)
            .map(|index| {
                let mut options = Vec::new();
                for path in returns {
                    let returned = path
                        .fixed
                        .get(index)
                        .copied()
                        .unwrap_or(InferredReturnType {
                            ty: self.primitives().nil,
                            table_literal: false,
                            preserve: false,
                        });
                    options.push(if returned.preserve {
                        // An explicit `:: T` return type is authoritative; keep it.
                        returned.ty
                    } else {
                        self.widen_inferred_return_type(
                            returned.ty,
                            seal_table_returns && returned.table_literal,
                        )
                    });
                }
                self.union_type(options)
            })
            .collect::<Vec<_>>();
        self.pack(types)
    }
    pub(crate) fn pack_requires_return_value(&self, pack: TypePackId) -> bool {
        let normalized = self.arena.normalize_pack(pack);
        // A `...T` tail accepts zero or more values, so a body that returns
        // nothing still satisfies it. Only fixed-arity types and unresolved
        // tails (free/generic/cyclic) force a returned value.
        normalized.types.iter().any(|ty| !self.is_dynamic(*ty))
            || matches!(
                normalized.tail,
                Some(TypePackTail::Free { .. } | TypePackTail::Generic(_) | TypePackTail::Cycle(_))
            )
    }
    pub(crate) fn bind_free_callee_to_function(
        &mut self,
        callee: TypeId,
        arguments: TypePackId,
        expected_returns: Option<TypePackId>,
    ) {
        let callee = self.arena.follow(callee);
        if !matches!(self.arena.get(callee), TypeKind::Free(_)) {
            return;
        }
        let returns = expected_returns.unwrap_or_else(|| self.arena.empty_pack());
        let function = self
            .arena
            .alloc(TypeKind::Function(FunctionType::new(arguments, returns)));
        self.bind_free_to(callee, function);
    }
    pub(crate) fn widen_inferred_return_type(
        &mut self,
        ty: TypeId,
        seal_table_returns: bool,
    ) -> TypeId {
        match self.arena.get(self.arena.follow(ty)).clone() {
            TypeKind::Singleton(SingletonType::Boolean(_)) => self.primitives().boolean,
            TypeKind::Singleton(SingletonType::String(_)) => self.primitives().string,
            TypeKind::Table(mut table) if seal_table_returns && table.is_unsealed() => {
                table.seal();
                self.arena.alloc(TypeKind::Table(table))
            }
            _ => ty,
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn expr_call(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: Option<Location>,
        func: &Expr,
        type_arguments: &[TypeParameter],
        args: &[Expr],
        is_self: bool,
    ) -> TypeId {
        if let Some(result) =
            self.require_return_types_expr_call(scope, expr, expr_ty, location, func, args)
        {
            return result;
        }
        let callee = self.expr_type(scope, func);
        if let Some(result) =
            self.degenerate_callee_expr_call(scope, expr, expr_ty, location, func, args, callee)
        {
            return result;
        }
        self.check_nilable_callee(callee, location);
        let expected_callee =
            self.resolved_expected_callee(scope, func, callee, type_arguments, location);
        if self.is_error_type(expected_callee) {
            for arg in args {
                self.expr_type(scope, arg);
            }
            self.bind_actual(location, expr.syntax_id(), expr_ty, self.primitives().error);
            return expr_ty;
        }
        let (arg_types, arg_tail, checked_callee) = if !is_self && is_table_insert_call(func) {
            let (arg_types, arg_tail) = self.table_insert_argument_types(scope, args);
            (arg_types, arg_tail, expected_callee)
        } else {
            self.call_argument_types(scope, func, args, is_self, expected_callee)
        };
        if self
            .calls
            .statement_call_results
            .contains(&expr.syntax_id())
        {
            self.constrain_missing_free_call_arguments_to_nil(
                expected_callee,
                arg_types.len(),
                func.location().map(DiagnosticLocation::from),
            );
        } else {
            self.bind_missing_free_call_arguments_to_nil(expected_callee, arg_types.len());
        }
        let discarding_results = self.calls.discard_call_results.contains(&expr.syntax_id());
        let recursive_return_placeholder = (!discarding_results
            && self.expr_is_current_recursive_call(expr))
        .then(|| {
            self.function_frames
                .recursive_return_placeholder_stack
                .last()
                .copied()
                .flatten()
        })
        .flatten();
        if !discarding_results {
            self.mark_recursive_value_call(func);
        }
        if let Some(result) = self.setmetatable_call_result(
            expr,
            expr_ty,
            location,
            func,
            args,
            &arg_types,
            discarding_results,
        ) {
            return result;
        }

        let supplied = CallArgumentSupply::from_parts(arg_types.len(), arg_tail);
        let call_arguments_are_concrete = arg_types.iter().all(|ty| !self.is_dynamic(*ty));
        self.apply_strict_table_insert_element_constraints(func, args, &arg_types);
        if let Some(result) = self.table_lifecycle_call_result(
            expr, expr_ty, location, func, args, &arg_types, arg_tail, is_self,
        ) {
            return result;
        }
        let format_call = self.string_format_call(func, args, is_self, &arg_types);
        let generic_pack_argument_types = arg_types.clone();
        let arguments = self.pack_with_tail(arg_types, arg_tail);
        let documentation_arguments = if is_self
            && let TypePackKind::List { types, tail } = self
                .arena
                .get_pack(self.arena.follow_pack(arguments))
                .clone()
        {
            self.arena.alloc_pack(TypePackKind::List {
                types: types.into_iter().skip(1).collect(),
                tail,
            })
        } else {
            arguments
        };
        self.generated.queries.record_call_arguments(
            expr.syntax_id(),
            arguments,
            documentation_arguments,
        );

        if let Some(format_call) = format_call {
            self.check_string_format_call(
                scope,
                location,
                expr.syntax_id(),
                expr_ty,
                &format_call,
                arg_tail,
            );
            return expr_ty;
        }

        if !is_self
            && matches!(func, Expr::Global { name, .. } if name.as_str() == "assert")
            && let Some((first_arg, first_arg_ty)) = args
                .first()
                .zip(generic_pack_argument_types.first().copied())
        {
            let result = self.assert_call_result_type(first_arg, first_arg_ty);
            self.bind_actual(location, expr.syntax_id(), expr_ty, result);
            return expr_ty;
        }

        if !is_self
            && is_select_global(func)
            && let Some(select_returns) =
                self.select_return_types(scope, args, &generic_pack_argument_types, arg_tail, 1)
            && let Some(select_result) = select_returns.first().copied()
        {
            self.bind_actual(location, expr.syntax_id(), expr_ty, select_result);
            return expr_ty;
        }

        // Fallback bidirectional expected-type propagation for `select` calls
        // whose argument sequence is not statically recoverable yet.
        if matches!(func, Expr::Global { name, .. } if name.as_str() == "select")
            && let Some(expected_result) = self.expected_by_syntax.get(&expr.syntax_id()).copied()
            && !self.is_dynamic(expected_result)
        {
            let expected_pack = self.pack(vec![expected_result]);
            self.generated.constraints.push(Constraint::call(
                callee,
                arguments,
                self.input.mode == Mode::Nonstrict,
                args.iter()
                    .map(|arg| arg.location().map(DiagnosticLocation::from))
                    .collect(),
                Some(expected_pack),
                func.location().map(DiagnosticLocation::from),
                true,
            ));
            self.bind_actual(location, expr.syntax_id(), expr_ty, expected_result);
            return expr_ty;
        }

        self.finish_call_constraint(
            expr,
            expr_ty,
            location,
            func,
            args,
            expected_callee,
            callee,
            checked_callee,
            supplied,
            discarding_results,
            recursive_return_placeholder,
            call_arguments_are_concrete,
            arguments,
            &generic_pack_argument_types,
            arg_tail,
        );
        expr_ty
    }

    /// Either constrains a dynamic callee (binding the result to `any`) or
    /// emits the regular call constraint for a concrete callee: reporting
    /// argument arity, choosing the expected/known return pack, and recording
    /// the call result.
    #[allow(clippy::too_many_arguments)]
    fn finish_call_constraint(
        &mut self,
        expr: &Expr,
        expr_ty: TypeId,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
        expected_callee: TypeId,
        callee: TypeId,
        checked_callee: TypeId,
        supplied: CallArgumentSupply,
        discarding_results: bool,
        recursive_return_placeholder: Option<TypeId>,
        call_arguments_are_concrete: bool,
        arguments: TypePackId,
        generic_pack_argument_types: &[TypeId],
        arg_tail: Option<TypePackId>,
    ) {
        if self.is_dynamic(expected_callee) {
            self.generated.constraints.push(Constraint::call(
                expected_callee,
                arguments,
                self.input.mode == Mode::Nonstrict,
                args.iter()
                    .map(|arg| arg.location().map(DiagnosticLocation::from))
                    .collect(),
                None,
                func.location().map(DiagnosticLocation::from),
                true,
            ));
            self.bind_actual(location, expr.syntax_id(), expr_ty, self.primitives().any);
        } else {
            let call_location = func.location().map(DiagnosticLocation::from);
            let call_result_pack = self.calls.call_result_packs.get(&expr.syntax_id()).copied();
            let generic_pack_argument_locations = args
                .iter()
                .map(|arg| arg.location().map(DiagnosticLocation::from))
                .collect::<Vec<_>>();
            let generic_pack_mismatch = self.report_generic_pack_call_argument_mismatch(
                expected_callee,
                generic_pack_argument_types,
                arg_tail,
                &generic_pack_argument_locations,
                call_location,
            );
            let arity_mismatch = generic_pack_mismatch
                || self.report_too_few_call_arguments(expected_callee, supplied, call_location);
            let callee_is_generic = self.function_is_generic(expected_callee);
            let expected_returns = if discarding_results {
                None
            } else if let Some(call_result_pack) = call_result_pack {
                Some(call_result_pack)
            } else if let Some(recursive_return_placeholder) = recursive_return_placeholder {
                Some(self.pack(vec![recursive_return_placeholder]))
            } else {
                Some(self.pack(vec![expr_ty]))
            };
            if self.expr_is_function_parameter(func)
                || self
                    .calls
                    .infer_discarded_call_callees
                    .contains(&expr.syntax_id())
                || (call_result_pack.is_some()
                    && matches!(self.arena.get(self.arena.follow(callee)), TypeKind::Free(_)))
            {
                self.bind_free_callee_to_function(callee, arguments, expected_returns);
            }
            let constraint_callee = if self.expr_is_function_parameter(func) {
                callee
            } else {
                checked_callee
            };
            // Generic functions can't use the known-return shortcut:
            // their `function.returns` references the unquantified
            // generic parameters, but the solver will instantiate
            // them to fresh free variables at the call site. We need
            // the constraint solver to fill expr_ty in with the
            // instantiated return, not the pre-instantiation generic.
            let known_return = if callee_is_generic {
                None
            } else if self.function_fixed_return_count(expected_callee) == Some(0)
                && call_arguments_are_concrete
            {
                Some(self.primitives().nil)
            } else {
                self.function_result_type(expected_callee)
            };
            let constraint_returns = if known_return.is_some() {
                None
            } else {
                expected_returns
            };
            if !arity_mismatch {
                self.generated.constraints.push(Constraint::call(
                    constraint_callee,
                    arguments,
                    self.input.mode == Mode::Nonstrict,
                    args.iter()
                        .map(|arg| arg.location().map(DiagnosticLocation::from))
                        .collect(),
                    constraint_returns,
                    call_location,
                    true,
                ));
            }
            if let Some(recursive_return_placeholder) = recursive_return_placeholder {
                self.bind_actual(
                    location,
                    expr.syntax_id(),
                    expr_ty,
                    recursive_return_placeholder,
                );
            } else if let Some(known_return) = known_return {
                self.bind_actual(location, expr.syntax_id(), expr_ty, known_return);
            } else {
                self.record_actual(location, expr.syntax_id(), expr_ty);
            }
        }
    }
    /// Returns the host-supplied required return type for this call statement,
    /// when the input pins it (after still typing the callee and arguments).
    fn require_return_types_expr_call(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
    ) -> Option<TypeId> {
        let return_types = self.input.require_return_types.get(&expr.syntax_id())?;
        self.expr_type(scope, func);
        for arg in args {
            self.expr_type(scope, arg);
        }
        let return_ty = return_types
            .first()
            .copied()
            .unwrap_or_else(|| self.primitives().any);
        self.bind_actual(location, expr.syntax_id(), expr_ty, return_ty);
        Some(expr_ty)
    }

    /// Handles a callee that resolves to `never` or to a top-function
    /// refinement: still type the arguments, then bind the result to
    /// `never`/`*error-type*` respectively.
    #[allow(clippy::too_many_arguments)]
    fn degenerate_callee_expr_call(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
        callee: TypeId,
    ) -> Option<TypeId> {
        if self.is_never_type(callee) {
            for arg in args {
                self.expr_type(scope, arg);
            }
            self.bind_actual(location, expr.syntax_id(), expr_ty, self.primitives().never);
            return Some(expr_ty);
        }
        if self.callee_is_top_function_refinement(func, callee) {
            for arg in args {
                self.expr_type(scope, arg);
            }
            self.report_top_function_refinement_call(func.location());
            self.bind_actual(location, expr.syntax_id(), expr_ty, self.primitives().error);
            return Some(expr_ty);
        }
        None
    }

    /// Computes the result of a `setmetatable(...)` call: the metatable-wrapped
    /// instance, the deferred `setmetatable` type-function for missing
    /// arguments, the dynamic-metatable shortcut, and the non-table-metatable
    /// diagnostic. Returns `None` for non-`setmetatable` callees (after still
    /// emitting the annotation recommendation when it applies).
    #[allow(clippy::too_many_arguments)]
    fn setmetatable_call_result(
        &mut self,
        expr: &Expr,
        expr_ty: TypeId,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
        arg_types: &[TypeId],
        discarding_results: bool,
    ) -> Option<TypeId> {
        if !matches!(func, Expr::Global { name, .. } if name.as_str() == "setmetatable") {
            return None;
        }
        self.report_setmetatable_call_annotation_recommendation(args);
        if let Some((&table, &metatable)) = arg_types.first().zip(arg_types.get(1))
            && (args
                .get(1)
                .is_some_and(|arg| self.should_preserve_setmetatable_result(arg, metatable))
                || self.expected_result_is_setmetatable_instance(expr.syntax_id()))
        {
            let result = self.arena.alloc(TypeKind::Metatable {
                table,
                metatable,
                name: None,
            });
            if discarding_results {
                self.apply_setmetatable_local_side_effect(args.first(), result);
            }
            self.bind_actual(location, expr.syntax_id(), expr_ty, result);
            return Some(expr_ty);
        }
        if args.len() < 2 {
            self.generated.diagnostics.push(Diagnostic::error(
                DiagnosticCategory::Call,
                func.location()
                    .or(location)
                    .map(DiagnosticLocation::from)
                    .unwrap_or_else(DiagnosticLocation::missing),
            ));
            let unknown = self.primitives().unknown;
            let table = arg_types.first().copied().unwrap_or(unknown);
            let metatable = arg_types.get(1).copied().unwrap_or(unknown);
            let result = self.arena.alloc(TypeKind::TypeFunctionInstance {
                name: SETMETATABLE_TYPE_FUNCTION.to_owned(),
                arguments: vec![table, metatable],
            });
            self.bind_free_to(expr_ty, result);
            self.record_actual(location, expr.syntax_id(), result);
            return Some(expr_ty);
        }
        if let Some((&table, &metatable)) = arg_types.first().zip(arg_types.get(1))
            && self.is_dynamic(metatable)
        {
            let result = if self.is_dynamic(table) {
                self.primitives().any
            } else {
                self.arena.alloc(TypeKind::Metatable {
                    table,
                    metatable,
                    name: None,
                })
            };
            if discarding_results {
                self.apply_setmetatable_local_side_effect(args.first(), result);
            }
            self.bind_actual(location, expr.syntax_id(), expr_ty, result);
            return Some(expr_ty);
        }
        // Diagnose a definitely-non-table metatable argument to `setmetatable`.
        // Only flag concrete scalar/function shapes — anything that could still
        // resolve to a table (free, generic, alias, metatable-wrapped, union,
        // intersection, …) is left to the regular subtyping path.
        if let Some(&mt) = arg_types.get(1)
            && matches!(
                self.arena.get(self.arena.follow(mt)),
                TypeKind::Primitive(_)
                    | TypeKind::Singleton(_)
                    | TypeKind::Function(_)
                    | TypeKind::Never
            )
        {
            self.generated.diagnostics.push(
                Diagnostic::error(
                    DiagnosticCategory::Call,
                    DiagnosticLocation::from_opt(location),
                )
                .with_context("Metatable was not a table".to_string()),
            );
            // Preserve the base table as the call result rather than collapsing
            // to `any`. Upstream leaks the bad-metatable result, so subsequent
            // property accesses on the value are still checked against the base
            // table shape.
            if let Some(&table) = arg_types.first() {
                if discarding_results {
                    self.apply_setmetatable_local_side_effect(args.first(), table);
                }
                self.bind_actual(location, expr.syntax_id(), expr_ty, table);
                return Some(expr_ty);
            }
        }
        None
    }

    /// Computes the result of a `table.freeze` / `table.clone` call, including
    /// the generic-vararg `table.freeze` error path.
    #[allow(clippy::too_many_arguments)]
    fn table_lifecycle_call_result(
        &mut self,
        expr: &Expr,
        expr_ty: TypeId,
        location: Option<Location>,
        func: &Expr,
        args: &[Expr],
        arg_types: &[TypeId],
        arg_tail: Option<TypePackId>,
        is_self: bool,
    ) -> Option<TypeId> {
        if !is_self && is_table_freeze_call(func) {
            if matches!(args, [arg] if matches!(ungroup_expr(arg), Expr::Varargs { .. }))
                && self.current_vararg_pack_is_generic()
            {
                self.report_table_freeze_call_error(location);
                self.bind_actual(location, expr.syntax_id(), expr_ty, self.primitives().any);
                return Some(expr_ty);
            }
            let result = self.table_freeze_result_type(location, args, arg_types, arg_tail);
            self.bind_actual(location, expr.syntax_id(), expr_ty, result);
            return Some(expr_ty);
        }
        if !is_self && is_table_clone_call(func) {
            let result = self.table_clone_result_type(arg_types, arg_tail);
            self.bind_actual(location, expr.syntax_id(), expr_ty, result);
            return Some(expr_ty);
        }
        None
    }

    fn string_format_call<'b>(
        &self,
        func: &'b Expr,
        args: &'b [Expr],
        is_self: bool,
        arg_types: &[TypeId],
    ) -> Option<StringFormatCall<'b>> {
        match ungroup_expr(func) {
            Expr::Local { local, .. }
                if self.local_surface.string_format_aliases.contains(&local.id) =>
            {
                Some(StringFormatCall::explicit(args, arg_types))
            }
            Expr::IndexName {
                expr, index, op, ..
            } if index.as_str() == "format" => {
                if *op == IndexOp::Colon
                    && is_self
                    && self.type_is_string_like(arg_types.first().copied())
                {
                    return Some(StringFormatCall {
                        format_expr: Some(expr.as_ref()),
                        format_ty: arg_types.first().copied(),
                        value_exprs: args,
                        supplied_count: args.len() + 1,
                    });
                }
                if *op == IndexOp::Dot
                    && (is_string_global(expr) || matches!(ungroup_expr(expr), Expr::String { .. }))
                {
                    return Some(StringFormatCall::explicit(args, arg_types));
                }
                None
            }
            _ => None,
        }
    }

    fn check_string_format_call(
        &mut self,
        scope: ScopeId,
        location: Option<Location>,
        syntax_id: SyntaxId,
        expr_ty: TypeId,
        call: &StringFormatCall<'_>,
        arg_tail: Option<TypePackId>,
    ) {
        self.bind_actual(location, syntax_id, expr_ty, self.primitives().string);
        let location = DiagnosticLocation::from_opt(location);
        if call.format_expr.is_none() {
            self.report_string_format_arity(location);
            return;
        }
        let Some(format_ty) = call.format_ty else {
            self.report_string_format_arity(location);
            return;
        };
        let format = match self.string_format_source(format_ty) {
            StringFormatSource::Literal(format) => format,
            StringFormatSource::DynamicString => {
                self.generated.diagnostics.push(
                    Diagnostic::error(DiagnosticCategory::Call, location).with_context(
                        "We cannot statically check the type of `string.format` when called \
                         with a format string that is not statically known.",
                    ),
                );
                return;
            }
            StringFormatSource::Unchecked => return,
            StringFormatSource::Other => {
                let mut diagnostic =
                    Diagnostic::type_mismatch("string", self.arena.summary(format_ty));
                diagnostic.primary_location = location;
                self.generated.diagnostics.push(diagnostic);
                return;
            }
        };

        let expected = string_format::expected_arguments(&format);
        let expected_count = 1 + expected.len();
        let supplied_count = self
            .string_format_effective_supplied_count(scope, call)
            .unwrap_or(call.supplied_count);
        if arg_tail.is_none() {
            if supplied_count != expected_count {
                if call.last_value_is_varargs() && supplied_count == expected_count + 1 {
                    // Raw `...` may contribute zero values, so a call that is
                    // otherwise exactly satisfied is not definitely too long.
                } else {
                    self.report_string_format_arity(location);
                    return;
                }
            }
        } else if supplied_count > expected_count {
            self.report_string_format_arity(location);
            return;
        }

        for (index, expected) in expected.iter().enumerate() {
            let Some(arg) = call.value_exprs.get(index) else {
                break;
            };
            let Some(actual) = self
                .input
                .dfg
                .expression(arg.syntax_id())
                .map(|def| self.input.dfg.get(def).ty)
            else {
                continue;
            };
            let expected_ty = match expected {
                FormatArgument::Any => continue,
                FormatArgument::String => self.primitives().string,
                FormatArgument::Number => self.primitives().number,
            };
            self.bind_function_parameter_expected_type(arg, expected_ty);
            if self.is_dynamic(actual)
                || Subtyper::new(self.arena)
                    .is_subtype(actual, expected_ty)
                    .is_ok()
            {
                continue;
            }
            let mut diagnostic = Diagnostic::type_mismatch(
                self.arena.summary(expected_ty),
                self.arena.summary(actual),
            );
            diagnostic.primary_location = DiagnosticLocation::from_opt(arg.location());
            self.generated.diagnostics.push(diagnostic);
        }
    }

    fn string_format_source(&self, ty: TypeId) -> StringFormatSource {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Singleton(SingletonType::String(value)) => {
                StringFormatSource::Literal(value.clone())
            }
            TypeKind::Primitive(PrimitiveType::String) => StringFormatSource::DynamicString,
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error => StringFormatSource::Unchecked,
            _ => StringFormatSource::Other,
        }
    }

    fn type_is_string_like(&self, ty: Option<TypeId>) -> bool {
        ty.is_some_and(|ty| {
            matches!(
                self.arena.get(self.arena.follow(ty)),
                TypeKind::Primitive(PrimitiveType::String)
                    | TypeKind::Singleton(SingletonType::String(_))
            )
        })
    }

    fn report_string_format_arity(&mut self, location: DiagnosticLocation) {
        self.generated
            .diagnostics
            .push(Diagnostic::error(DiagnosticCategory::Call, location));
    }

    fn string_format_effective_supplied_count(
        &mut self,
        scope: ScopeId,
        call: &StringFormatCall<'_>,
    ) -> Option<usize> {
        let last_value = call.value_exprs.last()?;
        let return_count = self.call_fixed_return_count_from_dfg(scope, last_value)?;
        Some(call.supplied_count.saturating_sub(1) + return_count)
    }

    fn report_setmetatable_call_annotation_recommendation(&mut self, args: &[Expr]) {
        let Some(call_function) = args
            .get(1)
            .and_then(setmetatable_call_metamethod_function_needing_annotation)
        else {
            return;
        };
        let Expr::Function { location, args, .. } = call_function else {
            return;
        };
        let function_ty = self.dfg_type_for_expr(call_function);
        let recommended_return = self
            .function_result_type(function_ty)
            .map(|ty| self.arena.summary(ty))
            .unwrap_or_else(|| "unknown".to_owned());
        let recommended_args: Vec<_> = args
            .iter()
            .filter(|arg| arg.annotation.is_none())
            .filter_map(|arg| {
                let ty = self
                    .input
                    .dfg
                    .local(arg.id)
                    .map(|def| self.input.dfg.get(def).ty)
                    .map(|ty| self.arena.summary(ty))?;
                Some(crate::diagnostics::RecommendedArgument {
                    name: arg.name.as_str().to_owned(),
                    ty,
                })
            })
            .collect();
        let diagnostic = Diagnostic::error(
            DiagnosticCategory::Generic,
            DiagnosticLocation::from_opt(*location),
        )
        .with_typed(Payload::ExplicitFunctionAnnotationRecommended {
            recommended_return: Some(recommended_return),
            recommended_args: Some(recommended_args),
        });
        self.generated.diagnostics.push(diagnostic);
    }

    fn expected_result_is_setmetatable_instance(&self, syntax_id: SyntaxId) -> bool {
        self.expected_by_syntax
            .get(&syntax_id)
            .is_some_and(|expected| {
                matches!(
                    self.arena.get(self.arena.follow(*expected)),
                    TypeKind::TypeFunctionInstance { name, .. }
                        if name == SETMETATABLE_TYPE_FUNCTION
                )
            })
    }

    fn mark_recursive_value_call(&mut self, func: &Expr) {
        match ungroup_expr(func) {
            Expr::Local { local, .. }
                if self
                    .function_frames
                    .local_function_stack
                    .last()
                    .copied()
                    .flatten()
                    .is_some_and(|current| current == local.id) =>
            {
                if let Some(seen) = self.function_frames.recursive_value_call_stack.last_mut() {
                    *seen = true;
                }
            }
            Expr::Global { name, .. }
                if self
                    .function_frames
                    .global_function_stack
                    .last()
                    .and_then(|current| current.as_deref())
                    .is_some_and(|current| current == name.as_str()) =>
            {
                if let Some(seen) = self.function_frames.recursive_value_call_stack.last_mut() {
                    *seen = true;
                }
            }
            _ => {}
        }
    }
    pub(crate) fn expr_is_current_recursive_call(&self, expr: &Expr) -> bool {
        let Expr::Call { func, .. } = ungroup_expr(expr) else {
            return false;
        };
        match ungroup_expr(func) {
            Expr::Local { local, .. } => self
                .function_frames
                .local_function_stack
                .last()
                .copied()
                .flatten()
                .is_some_and(|current| current == local.id),
            Expr::Global { name, .. } => self
                .function_frames
                .global_function_stack
                .last()
                .and_then(|current| current.as_deref())
                .is_some_and(|current| current == name.as_str()),
            _ => false,
        }
    }
    fn apply_strict_table_insert_element_constraints(
        &mut self,
        func: &Expr,
        args: &[Expr],
        arg_types: &[TypeId],
    ) {
        if self.input.mode != Mode::Strict || !is_table_insert_call(func) {
            return;
        }
        let Some(&table) = arg_types.first() else {
            return;
        };
        let (value_expr, value) = match (args, arg_types) {
            ([_, value], [_, value_ty]) => (value, *value_ty),
            ([_, _, value], [_, _, value_ty]) => (value, *value_ty),
            _ => return,
        };
        if self.is_dynamic(table) || self.is_dynamic(value) {
            return;
        }

        let table = self.arena.follow(table);
        let value = self.arena.follow(value);
        match self.arena.get(table).clone() {
            TypeKind::Free(_) => {
                self.replace_table_insert_indexer(table, TableState::Free, value);
            }
            TypeKind::Table(mut table_type)
                if matches!(table_type.state, TableState::Free | TableState::Unsealed) =>
            {
                if let Some(indexer) = &table_type.indexer {
                    let expected = self.arena.follow(indexer.value);
                    let location = value_expr.location().map(DiagnosticLocation::from);
                    let compatible = self
                        .push_strict_table_insert_exactness_constraints(value, expected, location);
                    if !compatible {
                        table_type.indexer = Some(TableIndexer {
                            key: self.primitives().number,
                            value,
                            read_only: false,
                        });
                        self.arena.replace(table, TypeKind::Table(table_type));
                    }
                    return;
                }
                table_type.indexer = Some(TableIndexer {
                    key: self.primitives().number,
                    value,
                    read_only: false,
                });
                self.arena.replace(table, TypeKind::Table(table_type));
            }
            _ => {}
        }
    }
    fn replace_table_insert_indexer(&mut self, table: TypeId, state: TableState, value: TypeId) {
        let mut table_type = TableType::new(state);
        table_type.indexer = Some(TableIndexer {
            key: self.primitives().number,
            value,
            read_only: false,
        });
        self.arena.replace(table, TypeKind::Table(table_type));
    }

    fn table_freeze_result_type(
        &mut self,
        location: Option<Location>,
        args: &[Expr],
        arg_types: &[TypeId],
        arg_tail: Option<TypePackId>,
    ) -> TypeId {
        let [arg] = arg_types else {
            if arg_tail.is_some_and(|tail| self.type_pack_is_dynamic(tail)) {
                return self.primitives().any;
            }
            if let Some((arg, arg_ty)) = args.first().zip(arg_types.first()) {
                let arg_ty = self.table_freeze_effective_argument_type(Some(arg), *arg_ty);
                self.report_table_freeze_argument_type_error_if_needed(
                    arg_ty,
                    arg.location().or(location),
                );
            }
            self.report_table_freeze_call_error(location);
            return self.primitives().any;
        };
        if let Some(tail) = arg_tail {
            if self.type_pack_is_dynamic(tail) {
                return self.primitives().any;
            }
            self.report_table_freeze_call_error(location);
            return self.primitives().any;
        }
        let arg = self.table_freeze_effective_argument_type(args.first(), *arg);
        if self.is_dynamic(arg) {
            return arg;
        }
        if self.type_is_definitely_not_freezable_table(arg) {
            self.report_table_freeze_call_error(location);
            return self.primitives().error;
        }
        self.table_freeze_readonly_result_type(arg).unwrap_or(arg)
    }

    fn table_clone_result_type(
        &mut self,
        arg_types: &[TypeId],
        arg_tail: Option<TypePackId>,
    ) -> TypeId {
        let [arg] = arg_types else {
            return self.primitives().any;
        };
        if arg_tail.is_some() {
            return self.primitives().any;
        }
        self.materialize_unsealed_property_writes_in_type(*arg);
        self.cloned_table_type(*arg).unwrap_or(*arg)
    }

    fn cloned_table_type(&mut self, ty: TypeId) -> Option<TypeId> {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Table(mut table) => {
                table.name = None;
                Some(self.arena.alloc(TypeKind::Table(table)))
            }
            TypeKind::Union(options) => {
                let options = options
                    .into_iter()
                    .map(|option| self.cloned_table_type(option).unwrap_or(option))
                    .collect();
                Some(self.arena.alloc(TypeKind::Union(options)))
            }
            TypeKind::Intersection(parts) => {
                let parts = parts
                    .into_iter()
                    .map(|part| self.cloned_table_type(part).unwrap_or(part))
                    .collect();
                Some(self.arena.alloc(TypeKind::Intersection(parts)))
            }
            _ => None,
        }
    }

    fn table_freeze_effective_argument_type(&self, arg: Option<&Expr>, ty: TypeId) -> TypeId {
        let ty = self.arena.follow(ty);
        if let Some(arg) = arg
            && let Some(local_id) = self.local_from_grouped_expr(arg)
            && self.nil_tracking.implicit_locals.contains(&local_id)
            && matches!(self.arena.get(ty), TypeKind::Free(_))
        {
            return self.primitives().nil;
        }
        ty
    }

    fn report_table_freeze_argument_type_error_if_needed(
        &mut self,
        arg: TypeId,
        location: Option<Location>,
    ) {
        if self.is_dynamic(arg) {
            return;
        }
        if self.type_is_definitely_not_freezable_table(arg) {
            self.report_table_freeze_call_error(location);
        }
    }

    fn table_freeze_readonly_result_type(&mut self, ty: TypeId) -> Option<TypeId> {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Table(mut table) => {
                let mut changed = false;
                for (name, property) in &mut table.properties {
                    if !name.starts_with("__") {
                        property.read_only = true;
                        changed = true;
                    }
                }
                if let Some(indexer) = table.indexer.as_mut() {
                    indexer.read_only = true;
                    changed = true;
                }
                if changed {
                    self.arena.replace(ty, TypeKind::Table(table));
                }
                Some(ty)
            }
            TypeKind::Union(options) => {
                let options = options
                    .into_iter()
                    .map(|option| {
                        self.table_freeze_readonly_result_type(option)
                            .unwrap_or(option)
                    })
                    .collect();
                Some(self.arena.alloc(TypeKind::Union(options)))
            }
            TypeKind::Intersection(parts) => {
                let parts = parts
                    .into_iter()
                    .map(|part| self.table_freeze_readonly_result_type(part).unwrap_or(part))
                    .collect();
                Some(self.arena.alloc(TypeKind::Intersection(parts)))
            }
            _ => None,
        }
    }

    fn report_table_freeze_call_error(&mut self, location: Option<Location>) {
        self.generated.diagnostics.push(Diagnostic::error(
            DiagnosticCategory::Call,
            DiagnosticLocation::from_opt(location),
        ));
    }

    fn type_is_definitely_not_freezable_table(&self, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Function(_)
            | TypeKind::Extern { .. }
            | TypeKind::Generic(_)
            | TypeKind::Never => true,
            TypeKind::Union(options) => options
                .iter()
                .all(|option| self.type_is_definitely_not_freezable_table(*option)),
            _ => false,
        }
    }

    fn type_pack_is_dynamic(&self, pack: TypePackId) -> bool {
        match self.arena.get_pack(self.arena.follow_pack(pack)) {
            TypePackKind::List { types, tail } => {
                types.iter().all(|ty| self.is_dynamic(*ty))
                    && tail.is_none_or(|tail| self.type_pack_is_dynamic(tail))
            }
            TypePackKind::Variadic { ty } => self.is_dynamic(*ty),
            TypePackKind::Free { .. } | TypePackKind::Error => true,
            TypePackKind::Generic(_) => false,
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
        }
    }

    fn current_vararg_pack_is_generic(&self) -> bool {
        self.function_frames
            .vararg_stack
            .last()
            .and_then(|pack| *pack)
            .is_some_and(|pack| {
                matches!(
                    self.arena.get_pack(self.arena.follow_pack(pack)),
                    TypePackKind::Generic(_)
                )
            })
    }

    fn push_strict_table_insert_exactness_constraints(
        &mut self,
        value: TypeId,
        expected: TypeId,
        location: Option<DiagnosticLocation>,
    ) -> bool {
        let mut compatible = true;
        if Subtyper::new(self.arena)
            .is_subtype(value, expected)
            .is_err()
        {
            compatible = false;
            self.generated
                .constraints
                .push(Constraint::expected_subtype(
                    value, expected, location, false,
                ));
        }
        if Subtyper::new(self.arena)
            .is_subtype(expected, value)
            .is_err()
        {
            compatible = false;
            self.generated
                .constraints
                .push(Constraint::expected_subtype(
                    expected, value, location, false,
                ));
        }
        compatible
    }
}

fn is_self_method_call_through_self(func: &Expr) -> bool {
    let Expr::IndexName { expr, op, .. } = ungroup_expr(func) else {
        return false;
    };
    if *op != IndexOp::Colon {
        return false;
    }
    matches!(ungroup_expr(expr), Expr::Local { local, .. } if local.name.as_str() == "self")
}

fn is_select_global(expr: &Expr) -> bool {
    matches!(ungroup_expr(expr), Expr::Global { name, .. } if name.as_str() == "select")
}

fn string_pattern_call<'a, 'b>(
    func: &'b Expr,
    args: &'a [Expr],
    is_self: bool,
) -> Option<(&'b str, &'a str)> {
    let method = string_lib_method(func).or_else(|| string_self_method(func, is_self))?;
    let pattern_index = usize::from(!is_self);
    let pattern = args.get(pattern_index).and_then(string_literal)?;
    Some((method, pattern))
}

fn string_self_method(func: &Expr, is_self: bool) -> Option<&str> {
    let Expr::IndexName {
        expr, index, op, ..
    } = ungroup_expr(func)
    else {
        return None;
    };
    if !is_self || *op != IndexOp::Colon || !matches!(index.as_str(), "find" | "match" | "gmatch") {
        return None;
    }
    matches!(
        ungroup_expr(expr),
        Expr::String { .. } | Expr::Local { .. } | Expr::Global { .. } | Expr::IndexName { .. }
    )
    .then(|| index.as_str())
}

fn select_start_argument(arg: Option<&Expr>) -> Option<SelectStart> {
    match arg.map(ungroup_expr)? {
        Expr::String { value, .. } if value == "#" => Some(SelectStart::Count),
        Expr::Number { value, .. } => {
            let value = value.as_f64()?.floor();
            value
                .is_finite()
                .then_some(SelectStart::From(value as isize))
        }
        _ => None,
    }
}

fn select_start_index(start: isize, fixed_len: usize) -> Option<usize> {
    match start.cmp(&0) {
        std::cmp::Ordering::Greater => Some(start as usize - 1),
        std::cmp::Ordering::Less => {
            let index = fixed_len as isize + start;
            (index >= 0).then_some(index as usize)
        }
        std::cmp::Ordering::Equal => None,
    }
}

struct StringFormatCall<'a> {
    format_expr: Option<&'a Expr>,
    format_ty: Option<TypeId>,
    value_exprs: &'a [Expr],
    supplied_count: usize,
}

impl<'a> StringFormatCall<'a> {
    fn explicit(args: &'a [Expr], arg_types: &[TypeId]) -> Self {
        let Some((format_expr, value_exprs)) = args.split_first() else {
            return Self {
                format_expr: None,
                format_ty: None,
                value_exprs: &[],
                supplied_count: 0,
            };
        };
        Self {
            format_expr: Some(format_expr),
            format_ty: arg_types.first().copied(),
            value_exprs,
            supplied_count: args.len(),
        }
    }

    fn last_value_is_varargs(&self) -> bool {
        self.value_exprs
            .last()
            .is_some_and(|expr| matches!(ungroup_expr(expr), Expr::Varargs { .. }))
    }
}

enum StringFormatSource {
    Literal(String),
    DynamicString,
    Unchecked,
    Other,
}
