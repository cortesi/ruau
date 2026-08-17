//! Expression constraint generation for single-module checking.

use std::collections::{BTreeMap, BTreeSet};

use ruau_syntax::{
    BinaryOp, CompoundAssignOp, Expr, IndexOp, LocalId, LocalRef, Location, Name, Stat, SyntaxId,
    TableItem, TableItemKind, Type, UnaryOp,
};

use crate::{
    ast_util::ungroup_expr,
    builtins::{string_primitive_property_type, vector_primitive_property_type},
    call_pack::{ExpectedCallParameterPack, ReceiverParameter},
    constraints::{Constraint, ConstraintSolveError},
    dfg::{RefinementKey, RefinementMap},
    diagnostics::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Payload},
    generation::{
        operator::{
            BinaryBinding, DeferredBinaryOperatorDiagnostic, DeferredUnaryOperatorDiagnostic,
            RelationalOperandKind, binary_metamethod_name, binary_operator_text,
            binary_type_function_name, equality_operator_text, invalid_length_operand_options,
            is_relational_operator, relational_operator_text,
        },
        state::{
            AssignmentValue, CapturedNilQueryRead, ExpressionConstraintGenerator,
            IndexExprLocations, IndexNameBinding,
        },
    },
    graph::Mode,
    member_access,
    normalize::simplify_type,
    scopes::{ScopeId, Symbol},
    subtype::{Subtyper, definitely_uninhabited_type},
    type_function::{Reduction, TypeFunctionRuntime},
    types::{
        Arena, PrimitiveType, SingletonType, TableIndexer, TableProperty, TableState, TableType,
        TypeId, TypeKind, TypePackId, extern_is_subtype, is_top_function_type,
    },
    unify::Unifier,
};

impl<'a> ExpressionConstraintGenerator<'a> {
    pub(crate) fn lvalue_type(&mut self, scope: ScopeId, expr: &Expr) -> TypeId {
        let expr_ty = self.dfg_type_for_expr(expr);
        match expr {
            Expr::IndexName {
                location,
                expr: base,
                index,
                ..
            } => {
                let base_ty = self.expr_type(scope, base);
                self.bind_index_name_write(
                    *location,
                    expr.syntax_id(),
                    expr_ty,
                    base_ty,
                    index.as_str(),
                );
                expr_ty
            }
            Expr::IndexExpr {
                location,
                expr: base,
                index,
                ..
            } => {
                let base_ty = self.expr_type(scope, base);
                let index_ty = self.expr_type(scope, index);
                self.record_contextual_index_key_query(base_ty, index, index_ty);
                self.bind_index_expr_write(*location, expr.syntax_id(), expr_ty, base_ty, index_ty);
                expr_ty
            }
            Expr::Group { expr, .. } => self.lvalue_type(scope, expr),
            _ => self.expr_type(scope, expr),
        }
    }
    pub(crate) fn expr_type(&mut self, scope: ScopeId, expr: &Expr) -> TypeId {
        let expr_ty = self.dfg_type_for_expr(expr);
        let syntax_id = expr.syntax_id();
        match expr {
            Expr::Nil { location, .. } => {
                self.bind_actual(*location, expr.syntax_id(), expr_ty, self.primitives().nil)
            }
            Expr::Bool {
                location, value, ..
            } => {
                let singleton = self
                    .arena
                    .alloc(TypeKind::Singleton(SingletonType::Boolean(*value)));
                self.bind_actual(*location, expr.syntax_id(), expr_ty, singleton);
            }
            Expr::Number { location, .. } | Expr::Integer { location, .. } => {
                self.bind_actual(
                    *location,
                    expr.syntax_id(),
                    expr_ty,
                    self.primitives().number,
                );
            }
            Expr::String {
                location, value, ..
            } => {
                let singleton = self
                    .arena
                    .alloc(TypeKind::Singleton(SingletonType::String(value.clone())));
                self.bind_actual(*location, expr.syntax_id(), expr_ty, singleton);
            }
            Expr::Global { location, name, .. } => {
                return self.expr_global(scope, expr, expr_ty, location, name);
            }
            Expr::Local {
                location, local, ..
            } => {
                return self.expr_local(scope, expr, expr_ty, location, local);
            }
            Expr::Varargs { location, .. } => {
                let ty = self
                    .vararg_type_at(0)
                    .unwrap_or_else(|| self.primitives().any);
                self.bind_actual(*location, expr.syntax_id(), expr_ty, ty);
            }
            Expr::Call {
                location,
                func,
                type_arguments,
                args,
                is_self,
                ..
            } => {
                return self.expr_call(
                    scope,
                    expr,
                    expr_ty,
                    *location,
                    func,
                    type_arguments,
                    args,
                    *is_self,
                );
            }
            Expr::Binary {
                location,
                op,
                left,
                right,
                ..
            } => {
                return self.expr_binary(scope, expr, expr_ty, location, op, left, right);
            }
            Expr::Unary {
                location,
                op,
                expr: operand_expr,
                ..
            } => {
                let operand = self.expr_type(scope, operand_expr);
                let operand_is_unannotated_parameter =
                    self.expr_is_unannotated_function_parameter_path(operand_expr);
                self.bind_unary(
                    *location,
                    syntax_id,
                    expr_ty,
                    *op,
                    operand,
                    operand_is_unannotated_parameter,
                );
            }
            Expr::IfElse {
                location,
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                return self.expr_if_else(
                    scope, expr, expr_ty, location, condition, true_expr, false_expr,
                );
            }
            Expr::TypeAssertion {
                location,
                expr: inner,
                annotation,
                ..
            } => {
                return self.expr_type_assertion(scope, expr, expr_ty, location, inner, annotation);
            }
            Expr::IndexName {
                location,
                expr: base,
                index,
                ..
            } => {
                return self.expr_index_name(scope, expr, expr_ty, location, base, index);
            }
            Expr::IndexExpr {
                location,
                expr: base,
                index,
                ..
            } => {
                return self.expr_index_expr(scope, expr, expr_ty, location, base, index);
            }
            Expr::Group { location, expr, .. } => {
                let inner_ty = self.expr_type(scope, expr);
                self.bind_actual(*location, expr.syntax_id(), expr_ty, inner_ty);
            }
            Expr::Instantiate {
                location,
                expr: inner,
                type_arguments,
                ..
            } => {
                let inner_ty = self.expr_type(scope, inner);
                let instantiated = if let Some(method) = explicit_table_builtin_method(inner) {
                    self.explicit_table_builtin_instantiation(
                        scope,
                        method,
                        type_arguments,
                        *location,
                    )
                } else {
                    self.explicit_type_instantiation(scope, inner_ty, type_arguments, *location)
                };
                self.bind_actual(*location, expr.syntax_id(), expr_ty, instantiated);
            }
            Expr::Table {
                location, items, ..
            } => self.expr_table(scope, expr, expr_ty, *location, items),
            Expr::InterpString {
                location,
                expressions,
                ..
            } => {
                for expr in expressions {
                    self.expr_type(scope, expr);
                }
                self.bind_actual(
                    *location,
                    expr.syntax_id(),
                    expr_ty,
                    self.primitives().string,
                );
            }
            Expr::Function {
                location,
                generics,
                generic_packs,
                args,
                self_arg,
                vararg,
                vararg_annotation,
                return_annotation,
                body,
                ..
            } => self.expr_function(
                scope,
                expr,
                expr_ty,
                *location,
                generics,
                generic_packs,
                args,
                self_arg.as_ref(),
                *vararg,
                vararg_annotation.as_deref(),
                return_annotation.as_deref(),
                body,
            ),
            Expr::Error {
                location,
                expressions,
                ..
            } => {
                for expr in expressions {
                    self.expr_type(scope, expr);
                }
                self.bind_actual(
                    *location,
                    expr.syntax_id(),
                    expr_ty,
                    self.primitives().error,
                );
            }
        }
        expr_ty
    }
    fn expr_global(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: &Option<Location>,
        name: &Name,
    ) -> TypeId {
        let key = RefinementKey::Symbol(Symbol::Global(name.as_str().to_owned()));
        let global_ty = self.refined_type(&key).or_else(|| {
            self.input
                .scopes
                .lookup_global(scope, name.as_str())
                .and_then(|binding| binding.ty)
                .or_else(|| self.generated.global_defs.get(name.as_str()).copied())
        });
        match global_ty {
            Some(global_ty) => self.bind_actual(*location, expr.syntax_id(), expr_ty, global_ty),
            None if self.input.mode == Mode::NoCheck
                || self
                    .unknown_symbols
                    .suppressed_global_reads
                    .contains(name.as_str()) =>
            {
                self.bind_actual(*location, expr.syntax_id(), expr_ty, self.primitives().any)
            }
            None => {
                self.report_unknown_symbol(
                    expr.syntax_id(),
                    name.as_str(),
                    DiagnosticLocation::from_opt(*location),
                );
                self.bind_actual(
                    *location,
                    expr.syntax_id(),
                    expr_ty,
                    self.primitives().error,
                );
            }
        }
        expr_ty
    }

    fn expr_local(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: &Option<Location>,
        local: &LocalRef,
    ) -> TypeId {
        let local_ty = self.refined_local_type(local.id).unwrap_or_else(|| {
            let ty = self
                .input
                .dfg
                .local(local.id)
                .map(|def| self.input.dfg.get(def).ty)
                .unwrap_or_else(|| self.recovery_type_at(*location, "missing local def"));
            if self.nil_tracking.local_starts_as_nil(local.id)
                && matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Free(_))
            {
                self.primitives().nil
            } else {
                ty
            }
        });
        self.bind_actual(
            *location,
            expr.syntax_id(),
            expr_ty,
            self.arena.follow(local_ty),
        );
        if self
            .query_capture
            .generic_contextual_callback_locals
            .contains(&local.id)
        {
            self.record_actual(*location, expr.syntax_id(), self.primitives().unknown);
        }
        self.record_captured_nil_query_read(scope, local.id, expr.syntax_id(), *location, &[]);
        expr_ty
    }

    fn record_captured_nil_query_read(
        &mut self,
        scope: ScopeId,
        local_id: LocalId,
        syntax_id: SyntaxId,
        location: Option<Location>,
        path: &[String],
    ) {
        if !self.local_is_captured_upvalue(scope, local_id) {
            return;
        }
        let Some(actual) = self.generated.queries.actual_by_syntax(syntax_id) else {
            return;
        };
        if !self.arena.is_nil(actual) {
            return;
        }
        self.query_capture
            .captured_nil_reads
            .entry(local_id)
            .or_default()
            .push(CapturedNilQueryRead {
                syntax_id,
                location,
                path: path.to_vec(),
            });
    }

    fn captured_upvalue_query_path(
        &self,
        scope: ScopeId,
        expr: &Expr,
    ) -> Option<(LocalId, Vec<String>)> {
        match ungroup_expr(expr) {
            Expr::Local { local, .. } if self.local_is_captured_upvalue(scope, local.id) => {
                Some((local.id, Vec::new()))
            }
            Expr::IndexName {
                expr: base, index, ..
            } => {
                let (local_id, mut path) = self.captured_upvalue_query_path(scope, base)?;
                path.push(index.as_str().to_owned());
                Some((local_id, path))
            }
            _ => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn expr_binary(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: &Option<Location>,
        op: &BinaryOp,
        left: &Expr,
        right: &Expr,
    ) -> TypeId {
        let expected = self.expected_by_syntax.get(&expr.syntax_id()).copied();
        let left_ty = self.expr_type(scope, left);
        let right_ty = match op {
            BinaryOp::And => {
                let refinements = self.truthy_refinements(left);
                self.refinements.locals.push(refinements);
                let right_ty = self.expr_type_with_expected(scope, right, expected);
                self.refinements.locals.pop();
                right_ty
            }
            BinaryOp::Or => {
                let refinements = self.falsy_refinements(left);
                self.refinements.locals.push(refinements);
                let right_ty = self.expr_type(scope, right);
                self.refinements.locals.pop();
                right_ty
            }
            _ => self.expr_type(scope, right),
        };
        if matches!(op, BinaryOp::Concat) {
            let primitives = self.primitives();
            let concat_operand = self.union_type(vec![primitives.string, primitives.number]);
            self.bind_concat_parameter_expected_type(left, right_ty, concat_operand);
            self.bind_concat_parameter_expected_type(right, left_ty, concat_operand);
        }
        self.bind_binary(&BinaryBinding {
            location: *location,
            syntax_id: expr.syntax_id(),
            expr_ty,
            op: *op,
            left: left_ty,
            right: right_ty,
            expected,
            unknown_global_parameter_operands: self
                .unknown_global_parameter_binary_operands(*op, left, right),
            unannotated_parameter_operands: self.expr_is_function_parameter_local(left)
                && self.expr_is_function_parameter_local(right),
            property_free_relational_operands: self
                .property_free_relational_operands(*op, left, right),
            recursive_call_operand: self.expr_is_current_recursive_call(left)
                || self.expr_is_current_recursive_call(right),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn expr_if_else(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: &Option<Location>,
        condition: &Expr,
        true_expr: &Expr,
        false_expr: &Expr,
    ) -> TypeId {
        let expected = self.expected_by_syntax.get(&expr.syntax_id()).copied();
        self.expr_type_in_refinement_context(scope, condition);
        let true_refinements = self.truthy_refinements(condition);
        self.refinements.locals.push(true_refinements);
        let truthy = self.expr_type_with_expected(scope, true_expr, expected);
        self.refinements.locals.pop();
        let false_refinements = self.falsy_refinements(condition);
        self.refinements.locals.push(false_refinements);
        let falsey = self.expr_type_with_expected(scope, false_expr, expected);
        self.refinements.locals.pop();
        let union = self.union_type(vec![truthy, falsey]);
        self.bind_actual(*location, expr.syntax_id(), expr_ty, union);
        expr_ty
    }

    fn expr_type_assertion(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: &Option<Location>,
        inner: &Expr,
        annotation: &Type,
    ) -> TypeId {
        let actual = self.expr_type(scope, inner);
        let annotation_ty = self.lower_type(scope, annotation);
        if self.type_assertion_needs_error(actual, annotation_ty) {
            self.generated
                .constraints
                .push(Constraint::subtype_default_location(
                    actual,
                    annotation_ty,
                    location.map(DiagnosticLocation::from),
                ));
        }
        self.bind_actual(*location, expr.syntax_id(), expr_ty, annotation_ty);
        expr_ty
    }

    fn expr_index_name(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: &Option<Location>,
        base: &Expr,
        index: &Name,
    ) -> TypeId {
        let base_ty = self.expr_type(scope, base);
        let grow_free_parameter_table = self.expr_is_unannotated_function_parameter_path(base);
        let grow_refinement_probe_table =
            self.refinements.property_probes.contains(&expr.syntax_id());
        if grow_free_parameter_table {
            self.bind_function_parameter_property_read_expectation(base, index.as_str(), expr_ty);
        }
        let read_ty = self.bind_index_name(&IndexNameBinding {
            location: *location,
            syntax_id: expr.syntax_id(),
            expr_ty,
            base_ty,
            index: index.as_str(),
            grow_free_parameter_table,
            grow_refinement_probe_table,
        });
        if let Some((local_id, mut path)) = self.captured_upvalue_query_path(scope, base) {
            path.push(index.as_str().to_owned());
            self.record_captured_nil_query_read(
                scope,
                local_id,
                expr.syntax_id(),
                *location,
                &path,
            );
        }
        read_ty
    }

    fn expr_index_expr(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: &Option<Location>,
        base: &Expr,
        index: &Expr,
    ) -> TypeId {
        let base_ty = self.expr_type(scope, base);
        let index_ty = self.expr_type(scope, index);
        let eager_read = !matches!(index, Expr::Number { .. } | Expr::Integer { .. });
        self.record_contextual_index_key_query(base_ty, index, index_ty);
        self.bind_index_expr(
            IndexExprLocations {
                expr: *location,
                index: index.location().map(DiagnosticLocation::from),
            },
            expr.syntax_id(),
            expr_ty,
            base_ty,
            index_ty,
            eager_read,
        );
        expr_ty
    }

    pub(crate) fn expr_type_with_expected(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        self.expr_type_with_expected_aggregation(scope, expr, expected, false)
    }

    pub(crate) fn expr_type_with_expected_aggregation(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expected: Option<TypeId>,
        aggregate_errors: bool,
    ) -> TypeId {
        if let Some(expected) = expected {
            self.expected_by_syntax.insert(expr.syntax_id(), expected);
        }
        let ty = self.expr_type(scope, expr);
        if expected.is_some() {
            self.expected_by_syntax.remove(&expr.syntax_id());
        }
        self.apply_expected_to_typed_expr(expr, ty, expected, aggregate_errors)
    }

    pub(crate) fn apply_expected_to_typed_expr(
        &mut self,
        expr: &Expr,
        ty: TypeId,
        expected: Option<TypeId>,
        aggregate_errors: bool,
    ) -> TypeId {
        if let Some(expected) = expected {
            let deferred_parameter_expected =
                self.bind_function_parameter_expected_type(expr, expected);
            if !deferred_parameter_expected
                && !self.is_dynamic(ty)
                && !self.is_error_type(expected)
                && !self.expected_accepts_without_subtype(ty, expected)
                && !self.nonstrict_union_expected_subtype_is_permissive(ty, expected)
            {
                self.generated
                    .constraints
                    .push(Constraint::expected_subtype(
                        ty,
                        expected,
                        expr.location().map(DiagnosticLocation::from),
                        aggregate_errors,
                    ));
            }
            self.generated.queries.record_expected(
                expr.syntax_id(),
                expr.location().map(DiagnosticLocation::from),
                expected,
            );
        }
        ty
    }

    pub(crate) fn expr_type_with_checked_call_expected(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expected: Option<TypeId>,
        checked_argument_rules_apply: bool,
    ) -> TypeId {
        if !checked_argument_rules_apply {
            return self.expr_type_with_expected_aggregation(scope, expr, expected, false);
        }
        self.nonstrict_checked_argument_depth += 1;
        let ty = self.expr_type_with_expected_aggregation(scope, expr, expected, true);
        self.nonstrict_checked_argument_depth -= 1;
        ty
    }

    pub(crate) fn nonstrict_union_expected_subtype_is_permissive(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> bool {
        if self.input.mode != Mode::Nonstrict {
            return false;
        }
        let actual = self.arena.follow(actual);
        if !matches!(self.arena.get(actual), TypeKind::Union(_)) {
            return false;
        }
        let mut has_matching_option = false;
        for option in self.arena.union_options(actual) {
            let option = self.arena.follow(option);
            if matches!(
                self.arena.get(option),
                TypeKind::Primitive(PrimitiveType::Nil)
            ) {
                return false;
            }
            has_matching_option |= Subtyper::new(self.arena)
                .is_subtype(option, expected)
                .is_ok();
        }
        has_matching_option
    }

    fn bind_concat_parameter_expected_type(
        &mut self,
        expr: &Expr,
        other_ty: TypeId,
        concat_operand: TypeId,
    ) -> bool {
        if !self.expr_is_unannotated_function_parameter_path(expr)
            || !self.type_is_known_concat_operand(other_ty, &mut BTreeSet::new())
        {
            return false;
        }
        self.bind_function_parameter_expected_type(expr, concat_operand)
    }

    fn type_is_known_concat_operand(&self, ty: TypeId, seen: &mut BTreeSet<TypeId>) -> bool {
        let ty = self.arena.follow(ty);
        if !seen.insert(ty) {
            return true;
        }
        match self.arena.get(ty) {
            TypeKind::Primitive(PrimitiveType::String | PrimitiveType::Number)
            | TypeKind::Singleton(SingletonType::String(_)) => true,
            TypeKind::Union(options) | TypeKind::Intersection(options) => options
                .iter()
                .all(|option| self.type_is_known_concat_operand(*option, seen)),
            TypeKind::Bound(bound) => self.type_is_known_concat_operand(*bound, seen),
            _ => false,
        }
    }

    pub(crate) fn bind_unary(
        &mut self,
        location: Option<Location>,
        syntax_id: SyntaxId,
        expr_ty: TypeId,
        op: UnaryOp,
        operand: TypeId,
        operand_is_unannotated_parameter: bool,
    ) {
        let primitives = self.primitives();
        match op {
            UnaryOp::Not => self.bind_actual(location, syntax_id, expr_ty, primitives.boolean),
            UnaryOp::Minus => {
                if self.is_never_type(operand) {
                    self.bind_actual(location, syntax_id, expr_ty, primitives.never);
                    return;
                }
                if self.is_vector_like(operand) {
                    self.bind_actual(location, syntax_id, expr_ty, primitives.vector);
                    return;
                }
                if let Some(result) = self.failed_unary_metamethod_type_function_result(
                    "__unm", "unm", operand, &location,
                ) {
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    return;
                }
                if self.push_unary_metamethod_call("__unm", operand, expr_ty, location) {
                    self.record_actual(location, syntax_id, expr_ty);
                    return;
                }
                if !self.arithmetic_operand_accepts_number(operand) {
                    self.report_unary_operator_mismatch("-", operand, "__unm", location);
                    self.bind_actual(location, syntax_id, expr_ty, primitives.number);
                    return;
                }
                self.generated.constraints.push(Constraint::subtype(
                    operand,
                    primitives.number,
                    None,
                ));
                self.bind_actual(location, syntax_id, expr_ty, primitives.number);
            }
            UnaryOp::Len => {
                if self.arena.is_optional(operand) && !self.is_dynamic(operand) {
                    self.generated.diagnostics.push(Diagnostic::error(
                        DiagnosticCategory::Operator,
                        DiagnosticLocation::from_opt(location),
                    ));
                    self.report_nilable_type_mismatch(operand, location);
                }
                if !self.arena.is_optional(operand) && !self.is_dynamic(operand) {
                    let invalid_options = invalid_length_operand_options(self.arena, operand);
                    if !invalid_options.is_empty() {
                        for invalid in &invalid_options {
                            self.report_unary_operator_mismatch("#", *invalid, "__len", location);
                        }
                        if matches!(
                            self.arena.get(self.arena.follow(operand)),
                            TypeKind::Union(_)
                        ) {
                            self.report_unary_operator_mismatch("#", operand, "__len", location);
                        }
                    }
                }
                if operand_is_unannotated_parameter {
                    self.generated.deferred_unary_operator_diagnostics.push(
                        DeferredUnaryOperatorDiagnostic {
                            op,
                            operand,
                            location: location.map(DiagnosticLocation::from),
                        },
                    );
                }
                self.bind_actual(location, syntax_id, expr_ty, primitives.number)
            }
        }
    }

    fn report_unary_operator_mismatch(
        &mut self,
        operator: &str,
        operand: TypeId,
        overload: &str,
        location: Option<Location>,
    ) {
        let mut diagnostic =
            Diagnostic::unary_operator_error(operator, self.arena.summary(operand), overload);
        diagnostic.primary_location = DiagnosticLocation::from_opt(location);
        self.generated.diagnostics.push(diagnostic);
    }

    pub(crate) fn bind_binary(&mut self, binary: &BinaryBinding) -> TypeId {
        let &BinaryBinding {
            location,
            syntax_id,
            expr_ty,
            op,
            left,
            right,
            expected,
            unknown_global_parameter_operands,
            unannotated_parameter_operands,
            property_free_relational_operands,
            recursive_call_operand,
        } = binary;
        let primitives = self.primitives();
        match op {
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::FloorDiv
            | BinaryOp::Mod
            | BinaryOp::Pow => {
                let recursive_unannotated_arithmetic = recursive_call_operand
                    && self
                        .function_frames
                        .function_has_unannotated_parameter_stack
                        .last()
                        .copied()
                        .unwrap_or(false);
                if recursive_unannotated_arithmetic {
                    self.operator.recursive_arithmetic_exprs.insert(syntax_id);
                }
                if self.is_never_type(left) || self.is_never_type(right) {
                    self.operator.never_arithmetic_exprs.insert(syntax_id);
                    self.bind_actual(location, syntax_id, expr_ty, primitives.never);
                    return primitives.never;
                }
                if self.type_is_uninhabited(left) || self.type_is_uninhabited(right) {
                    self.operator.never_arithmetic_exprs.insert(syntax_id);
                    if let Some(result) = self
                        .pending_arithmetic_result_for_refined_uninhabited_operand(op, left, right)
                    {
                        self.bind_actual(location, syntax_id, expr_ty, result);
                        return result;
                    }
                    self.bind_actual(location, syntax_id, expr_ty, primitives.never);
                    return primitives.never;
                }
                if let Some(result) = self.vector_arithmetic_result(op, left, right) {
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    return result;
                }
                if self.is_any_type(left) || self.is_any_type(right) {
                    self.bind_actual(location, syntax_id, expr_ty, primitives.any);
                    return primitives.any;
                }
                if unknown_global_parameter_operands {
                    if let Some(result) = self.deferred_unknown_global_parameter_arithmetic_result(
                        op, left, right, location,
                    ) {
                        self.bind_actual(location, syntax_id, expr_ty, result);
                        return result;
                    }
                    self.report_unknown_global_parameter_binary_operator(op, location);
                    self.bind_actual(location, syntax_id, expr_ty, primitives.number);
                    return primitives.number;
                }
                if recursive_unannotated_arithmetic
                    && let Some(result) = self.pending_binary_type_function_result(op, left, right)
                {
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    return result;
                }
                if self.report_unknown_binary_operator(op, left, right, location) {
                    self.bind_actual(location, syntax_id, expr_ty, primitives.number);
                    return primitives.number;
                }
                if let Some(result) =
                    self.failed_binary_metamethod_type_function_result(&op, left, right, &location)
                {
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    return result;
                }
                if self.push_binary_metamethod_call(op, left, right, expr_ty, location) {
                    self.record_actual(location, syntax_id, expr_ty);
                    return expr_ty;
                }
                if self.report_invalid_arithmetic_operand(op, left, right, location) {
                    self.bind_actual(location, syntax_id, expr_ty, primitives.number);
                    return primitives.number;
                }
                if let Some(result) =
                    self.expected_add_type_function_result(op, left, right, expected)
                {
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    return result;
                }
                let named_local_function = self
                    .function_frames
                    .local_function_stack
                    .last()
                    .is_some_and(|local| local.is_some());
                if expected.is_none()
                    && unannotated_parameter_operands
                    && named_local_function
                    && let Some(result) = self
                        .pending_binary_type_function_result_for_indeterminate_operands(
                            op, left, right,
                        )
                {
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    return result;
                }
                if !self.is_dynamic(left) && !self.is_metatable_type(left) {
                    self.generated.constraints.push(Constraint::subtype(
                        left,
                        primitives.number,
                        None,
                    ));
                }
                if !self.is_dynamic(right) && !self.is_metatable_type(right) {
                    self.generated.constraints.push(Constraint::subtype(
                        right,
                        primitives.number,
                        None,
                    ));
                }
                self.bind_actual(location, syntax_id, expr_ty, primitives.number);
                primitives.number
            }
            BinaryOp::Concat => {
                if self.is_never_type(left) || self.is_never_type(right) {
                    self.bind_actual(location, syntax_id, expr_ty, primitives.never);
                    return primitives.never;
                }
                if unknown_global_parameter_operands {
                    self.report_unknown_global_parameter_binary_operator(op, location);
                    self.bind_actual(location, syntax_id, expr_ty, primitives.string);
                    return primitives.string;
                }
                if self.report_unknown_binary_operator(op, left, right, location) {
                    self.bind_actual(location, syntax_id, expr_ty, primitives.string);
                    return primitives.string;
                }
                if self.push_binary_metamethod_call(op, left, right, expr_ty, location) {
                    self.record_actual(location, syntax_id, expr_ty);
                    return expr_ty;
                }
                if let Some(result) = self
                    .pending_binary_type_function_result_for_indeterminate_operands(op, left, right)
                {
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    return result;
                }
                let concat_operand = self.union_type(vec![primitives.string, primitives.number]);
                // Nonstrict mode tolerates an optional operand, mirroring the
                // relational operators: a `nil` possibility is the caller's
                // concern, so strip it before the operand check rather than
                // reporting it.
                let (left, right) = if self.input.mode == Mode::Nonstrict {
                    (self.strip_nil(left), self.strip_nil(right))
                } else {
                    (left, right)
                };
                if !self.is_dynamic(left) {
                    self.generated.constraints.push(Constraint::subtype(
                        left,
                        concat_operand,
                        None,
                    ));
                }
                if !self.is_dynamic(right) {
                    self.generated.constraints.push(Constraint::subtype(
                        right,
                        concat_operand,
                        None,
                    ));
                }
                self.bind_actual(location, syntax_id, expr_ty, primitives.string);
                primitives.string
            }
            BinaryOp::CompareEq | BinaryOp::CompareNe => {
                self.check_equality_operands(op, left, right, location);
                self.bind_actual(location, syntax_id, expr_ty, primitives.boolean);
                primitives.boolean
            }
            BinaryOp::CompareLt
            | BinaryOp::CompareLe
            | BinaryOp::CompareGt
            | BinaryOp::CompareGe => {
                if self.push_binary_metamethod_call(op, left, right, primitives.boolean, location) {
                    self.bind_actual(location, syntax_id, expr_ty, primitives.boolean);
                    return primitives.boolean;
                }
                self.check_relational_operands(
                    op,
                    location,
                    left,
                    right,
                    property_free_relational_operands,
                );
                self.bind_actual(location, syntax_id, expr_ty, primitives.boolean);
                primitives.boolean
            }
            BinaryOp::And => match self.truthiness(left) {
                Truthiness::AlwaysTruthy => {
                    let result = self.logical_result_part_with_expected(right, expected);
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    result
                }
                Truthiness::AlwaysFalsy => {
                    let result = self.falsy_part(left);
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    result
                }
                Truthiness::Unknown => {
                    let falsy = self.falsy_part(left);
                    let right = self.logical_result_part_with_expected(right, expected);
                    let union = self.union_type(vec![falsy, right]);
                    self.bind_actual(location, syntax_id, expr_ty, union);
                    union
                }
            },
            BinaryOp::Or => match self.truthiness(left) {
                Truthiness::AlwaysTruthy => {
                    let truthy = self.truthy_part(left);
                    let right = self.logical_result_part_with_expected(right, expected);
                    let result = self.union_type(vec![truthy, right]);
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    result
                }
                Truthiness::AlwaysFalsy => {
                    let result = self.logical_result_part_with_expected(right, expected);
                    self.bind_actual(location, syntax_id, expr_ty, result);
                    result
                }
                Truthiness::Unknown => {
                    let truthy = self.truthy_part(left);
                    let right = self.logical_result_part_with_expected(right, expected);
                    let union = self.union_type(vec![truthy, right]);
                    self.bind_actual(location, syntax_id, expr_ty, union);
                    union
                }
            },
        }
    }
    fn report_unknown_binary_operator(
        &mut self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
        location: Option<Location>,
    ) -> bool {
        if !self.is_unknown_type(left) || !self.is_unknown_type(right) {
            return false;
        }
        let left = self.arena.summary(left);
        let right = self.arena.summary(right);
        self.push_binary_operator_diagnostic(op, left, right, location);
        true
    }
    fn deferred_unknown_global_parameter_arithmetic_result(
        &mut self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
        location: Option<Location>,
    ) -> Option<TypeId> {
        if op != BinaryOp::Add {
            return None;
        }
        self.generated.deferred_binary_operator_diagnostics.push(
            DeferredBinaryOperatorDiagnostic {
                op,
                left,
                right,
                location: location.map(DiagnosticLocation::from),
                global_function_name: self
                    .function_frames
                    .global_function_stack
                    .last()
                    .and_then(|name| name.clone()),
            },
        );
        Some(self.arena.alloc(TypeKind::TypeFunctionInstance {
            name: "add".to_owned(),
            arguments: vec![left, right],
        }))
    }
    fn report_unknown_global_parameter_binary_operator(
        &mut self,
        op: BinaryOp,
        location: Option<Location>,
    ) {
        self.push_binary_operator_diagnostic(op, "unknown", "unknown", location);
    }
    fn push_binary_operator_diagnostic(
        &mut self,
        op: BinaryOp,
        left: impl Into<String>,
        right: impl Into<String>,
        location: Option<Location>,
    ) {
        let overload = binary_metamethod_name(op).unwrap_or("operator");
        let mut diagnostic =
            Diagnostic::binary_operator_error(binary_operator_text(op), left, right, overload);
        if let Some(location) = location {
            diagnostic.primary_location = DiagnosticLocation::from(location);
        }
        self.generated.diagnostics.push(diagnostic);
    }
    fn failed_binary_metamethod_type_function_result(
        &mut self,
        op: &BinaryOp,
        left: TypeId,
        right: TypeId,
        location: &Option<Location>,
    ) -> Option<TypeId> {
        let (callee, arguments) = self.binary_metamethod_call(*op, left, right)?;
        if !self.nongeneric_call_target(callee) {
            return None;
        }
        let arguments = self.pack(arguments);
        let Err(error @ crate::overload::OverloadError::NoMatch { .. }) =
            crate::overload::resolve_call_for_constraint(
                self.arena,
                callee,
                arguments,
                true,
                self.input.mode == Mode::Nonstrict,
                false,
            )
        else {
            return None;
        };
        let result = self.pending_binary_type_function_result(*op, left, right)?;
        let diagnostic = ConstraintSolveError::Overload(error)
            .with_location(location.map(DiagnosticLocation::from))
            .into_diagnostic_with_arena(Some(&*self.arena));
        self.generated.diagnostics.push(diagnostic);
        Some(result)
    }
    fn failed_unary_metamethod_type_function_result(
        &mut self,
        metamethod: &str,
        type_function: &str,
        operand: TypeId,
        location: &Option<Location>,
    ) -> Option<TypeId> {
        let callee = self.type_metamethod(operand, metamethod)?;
        if !self.nongeneric_call_target(callee) {
            return None;
        }
        let arguments = self.pack(vec![operand]);
        let Err(error @ crate::overload::OverloadError::NoMatch { .. }) =
            crate::overload::resolve_call_for_constraint(
                self.arena,
                callee,
                arguments,
                true,
                self.input.mode == Mode::Nonstrict,
                false,
            )
        else {
            return None;
        };
        let arguments = vec![self.type_function_operator_operand(operand)];
        if TypeFunctionRuntime::new().reduce(self.arena, type_function, &arguments)
            != Reduction::Pending
        {
            return None;
        }
        let result = self.arena.alloc(TypeKind::TypeFunctionInstance {
            name: type_function.to_owned(),
            arguments,
        });
        self.generated
            .diagnostics
            .push(Diagnostic::uninhabited_type_function(
                self.arena.summary(result),
                DiagnosticLocation::from_opt(*location),
            ));
        let diagnostic = ConstraintSolveError::Overload(error)
            .with_location(location.map(DiagnosticLocation::from))
            .into_diagnostic_with_arena(Some(&*self.arena));
        self.generated.diagnostics.push(diagnostic);
        Some(result)
    }
    fn nongeneric_call_target(&self, callee: TypeId) -> bool {
        match self.arena.get(self.arena.follow(callee)) {
            TypeKind::Function(function) => {
                function.generics.is_empty() && function.generic_packs.is_empty()
            }
            TypeKind::Intersection(types) | TypeKind::Union(types) => {
                types.iter().all(|ty| self.nongeneric_call_target(*ty))
            }
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            _ => false,
        }
    }
    fn pending_binary_type_function_result(
        &mut self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
    ) -> Option<TypeId> {
        let name = binary_type_function_name(op)?;
        let arguments = vec![
            self.type_function_operator_operand(left),
            self.type_function_operator_operand(right),
        ];
        if TypeFunctionRuntime::new().reduce(self.arena, name, &arguments) != Reduction::Pending {
            return None;
        }
        Some(self.arena.alloc(TypeKind::TypeFunctionInstance {
            name: name.to_owned(),
            arguments,
        }))
    }
    fn pending_binary_type_function_result_for_indeterminate_operands(
        &mut self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
    ) -> Option<TypeId> {
        if !self.type_function_operand_is_indeterminate(left)
            && !self.type_function_operand_is_indeterminate(right)
        {
            return None;
        }
        self.pending_binary_type_function_result(op, left, right)
    }
    fn pending_arithmetic_result_for_refined_uninhabited_operand(
        &mut self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
    ) -> Option<TypeId> {
        if (self.type_is_refined_uninhabited(left)
            && self.type_function_operand_is_indeterminate(right))
            || (self.type_is_refined_uninhabited(right)
                && self.type_function_operand_is_indeterminate(left))
        {
            return self.pending_binary_type_function_result(op, left, right);
        }
        None
    }
    fn type_is_uninhabited(&self, ty: TypeId) -> bool {
        definitely_uninhabited_type(self.arena, ty)
    }
    fn type_is_refined_uninhabited(&self, ty: TypeId) -> bool {
        !self.is_never_type(ty) && self.type_is_uninhabited(ty)
    }
    fn type_function_operand_is_indeterminate(&self, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Free(_)
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::TypeFunctionInstance { .. } => true,
            TypeKind::Union(types) | TypeKind::Intersection(types) => types
                .iter()
                .any(|ty| self.type_function_operand_is_indeterminate(*ty)),
            TypeKind::Negation(inner) => self.type_function_operand_is_indeterminate(*inner),
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            _ => false,
        }
    }
    fn type_function_operator_operand(&self, ty: TypeId) -> TypeId {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Singleton(SingletonType::Boolean(_)) => self.primitives().boolean,
            TypeKind::Singleton(SingletonType::String(_)) => self.primitives().string,
            _ => ty,
        }
    }
    fn report_invalid_arithmetic_operand(
        &mut self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
        location: Option<Location>,
    ) -> bool {
        if self.arithmetic_operand_accepts_number(left)
            && self.arithmetic_operand_accepts_number(right)
        {
            return false;
        }
        self.push_binary_operator_diagnostic(
            op,
            self.arena.summary(left),
            self.arena.summary(right),
            location,
        );
        true
    }
    fn arithmetic_operand_accepts_number(&self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty) {
            TypeKind::Primitive(PrimitiveType::Number)
            | TypeKind::Primitive(PrimitiveType::Vector)
            | TypeKind::Metatable { .. }
            | TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Error
            | TypeKind::Blocked(_)
            | TypeKind::Free(_)
            | TypeKind::Generic(_)
            | TypeKind::Never => true,
            TypeKind::Union(types) => types
                .iter()
                .any(|option| self.arithmetic_operand_accepts_number(*option)),
            TypeKind::Intersection(types) => types
                .iter()
                .any(|option| self.arithmetic_operand_accepts_number(*option)),
            _ => false,
        }
    }
    fn is_unknown_type(&self, ty: TypeId) -> bool {
        matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Unknown)
    }
    fn unknown_global_parameter_binary_operands(
        &self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
    ) -> bool {
        if !matches!(
            op,
            BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::FloorDiv
                | BinaryOp::Mod
                | BinaryOp::Pow
                | BinaryOp::Concat
        ) {
            return false;
        }
        if !self
            .function_frames
            .function_is_global_stack
            .last()
            .copied()
            .unwrap_or(false)
        {
            return false;
        }
        self.expr_is_function_parameter_local(left) && self.expr_is_function_parameter_local(right)
    }
    fn property_free_relational_operands(&self, op: BinaryOp, left: &Expr, right: &Expr) -> bool {
        matches!(
            op,
            BinaryOp::CompareLt | BinaryOp::CompareLe | BinaryOp::CompareGt | BinaryOp::CompareGe
        ) && self.expr_is_index_access(left)
            && self.expr_is_index_access(right)
    }
    fn expr_is_index_access(&self, expr: &Expr) -> bool {
        match expr {
            Expr::IndexName { .. } | Expr::IndexExpr { .. } => true,
            Expr::Group { expr, .. } => self.expr_is_index_access(expr),
            _ => false,
        }
    }
    pub(crate) fn should_preserve_setmetatable_result(
        &self,
        metatable_expr: &Expr,
        metatable: TypeId,
    ) -> bool {
        let metatable = self.arena.follow(metatable);
        if matches!(metatable_expr, Expr::Local { .. })
            && !matches!(
                self.arena.get(metatable),
                TypeKind::Any | TypeKind::Unknown | TypeKind::Error
            )
        {
            return true;
        }
        if matches!(ungroup_expr(metatable_expr), Expr::Table { .. }) {
            return true;
        }
        let TypeKind::Table(table) = self.arena.get(metatable) else {
            return matches!(self.arena.get(metatable), TypeKind::Metatable { .. });
        };
        table.properties.contains_key("__call")
            || table.properties.contains_key("__index")
            || table.properties.contains_key("__iter")
            || self
                .table_writes
                .unsealed_property_writes
                .get(&metatable)
                .is_some_and(|properties| {
                    properties.keys().any(|name| {
                        matches!(name.as_str(), "__call" | "__index" | "__iter")
                            || is_operator_metamethod_name(name)
                    })
                })
            || table.properties.iter().any(|(name, property)| {
                is_operator_metamethod_name(name)
                    && !matches!(
                        self.arena.get(self.arena.follow(property.ty)),
                        TypeKind::Intersection(_)
                    )
            })
    }
    pub(crate) fn apply_setmetatable_local_side_effect(
        &mut self,
        target: Option<&Expr>,
        result: TypeId,
    ) {
        let Some(Expr::Local { local, .. }) = target else {
            return;
        };
        let is_annotated = self.local_surface.annotated_locals.contains(&local.id);
        if !is_annotated
            && !self
                .local_surface
                .setmetatable_side_effect_locals
                .contains(&local.id)
        {
            return;
        }
        let Some(def) = self.input.dfg.local(local.id) else {
            return;
        };
        let local_ty = self.input.dfg.get(def).ty;
        if !is_annotated {
            self.arena.bind_type(local_ty, result);
        }
        self.merge_current_refinements(RefinementMap::from([(
            RefinementKey::Symbol(Symbol::Local(local.id)),
            result,
        )]));
    }
    pub(crate) fn bind_index_name(&mut self, binding: &IndexNameBinding<'_>) -> TypeId {
        let &IndexNameBinding {
            location,
            syntax_id,
            expr_ty,
            base_ty,
            index,
            grow_free_parameter_table,
            grow_refinement_probe_table,
        } = binding;
        if self.is_never_type(base_ty) {
            self.bind_actual(location, syntax_id, expr_ty, self.primitives().never);
            return self.primitives().never;
        }
        if self.is_error_type(base_ty) {
            let error = self.primitives().error;
            self.bind_actual(location, syntax_id, expr_ty, error);
            return error;
        }
        if self.is_dynamic(base_ty) {
            self.bind_actual(location, syntax_id, expr_ty, self.primitives().any);
            return self.primitives().any;
        }
        if let Some(read_ty) = self.property_type(base_ty, index) {
            self.bind_actual(location, syntax_id, expr_ty, read_ty);
            return read_ty;
        }
        if grow_free_parameter_table
            && self.insert_free_parameter_read_property(base_ty, index, expr_ty)
        {
            self.record_actual(location, syntax_id, expr_ty);
            return expr_ty;
        }
        if grow_refinement_probe_table
            && self.insert_refinement_probe_property(base_ty, index, expr_ty)
        {
            self.record_actual(location, syntax_id, expr_ty);
            return expr_ty;
        }
        // Nonstrict indexing of a value that cannot carry user properties (a
        // bare scalar, non-string singleton, or function) yields `any` rather
        // than constraining the base to grow the property. Reaching here means
        // `property_type` found nothing and the base will not grow (free bases
        // and refinement probes are handled above), so the strict ReadProperty
        // constraint would spuriously reject lenient code like `f():andThen()`
        // where `f()` is a number (`check_function_before_lambda_that_uses_it`).
        if self.input.mode == Mode::Nonstrict
            && matches!(
                self.arena.get(self.arena.follow(base_ty)),
                TypeKind::Primitive(_) | TypeKind::Singleton(_) | TypeKind::Function(_)
            )
        {
            self.bind_actual(location, syntax_id, expr_ty, self.primitives().any);
            return self.primitives().any;
        }
        self.generated.constraints.push(Constraint::read_property(
            base_ty,
            index.to_owned(),
            expr_ty,
            location.map(DiagnosticLocation::from),
        ));
        self.record_actual(location, syntax_id, expr_ty);
        expr_ty
    }
    pub(crate) fn insert_free_parameter_read_property(
        &mut self,
        table: TypeId,
        name: &str,
        value: TypeId,
    ) -> bool {
        let table = self.arena.follow(table);
        match self.arena.get_mut(table) {
            kind @ TypeKind::Free(_) => {
                let mut table_type = TableType::new(TableState::Free);
                table_type
                    .properties
                    .insert(name.to_owned(), TableProperty::read_only(value));
                *kind = TypeKind::Table(table_type);
                true
            }
            TypeKind::Table(table_type) if table_type.state == TableState::Free => {
                table_type
                    .properties
                    .insert(name.to_owned(), TableProperty::read_only(value));
                true
            }
            _ => false,
        }
    }
    pub(crate) fn insert_refinement_probe_property(
        &mut self,
        table: TypeId,
        name: &str,
        value: TypeId,
    ) -> bool {
        self.insert_refinement_probe_property_with_seen(table, name, value, &mut Vec::new())
    }

    fn insert_refinement_probe_property_with_seen(
        &mut self,
        table: TypeId,
        name: &str,
        value: TypeId,
        seen: &mut Vec<TypeId>,
    ) -> bool {
        let table = self.arena.follow(table);
        if seen.contains(&table) {
            return false;
        }
        seen.push(table);
        let options = match self.arena.get_mut(table) {
            TypeKind::Free(_) => {
                return self.insert_free_parameter_read_property(table, name, value);
            }
            TypeKind::Table(table_type)
                if table_type.state == TableState::Free
                    && table_type.properties.is_empty()
                    && table_type.indexer.is_none() =>
            {
                table_type
                    .properties
                    .insert(name.to_owned(), TableProperty::read_only(value));
                return true;
            }
            TypeKind::Union(options) | TypeKind::Intersection(options) => options.clone(),
            _ => return false,
        };
        let mut inserted = false;
        for option in options {
            inserted |= self.insert_refinement_probe_property_with_seen(option, name, value, seen);
        }
        inserted
    }
    pub(crate) fn bind_index_name_write(
        &mut self,
        location: Option<Location>,
        syntax_id: SyntaxId,
        expr_ty: TypeId,
        base_ty: TypeId,
        index: &str,
    ) {
        if self.is_never_type(base_ty) {
            self.bind_actual(location, syntax_id, expr_ty, self.primitives().never);
            return;
        }
        if self.is_dynamic(base_ty) {
            self.bind_actual(location, syntax_id, expr_ty, self.primitives().any);
            return;
        }
        if self.report_known_non_table_property_write(base_ty, location) {
            self.record_actual(location, syntax_id, expr_ty);
            return;
        }
        self.generated.constraints.push(Constraint::write_property(
            base_ty,
            index.to_owned(),
            expr_ty,
            location.map(DiagnosticLocation::from),
        ));
        self.record_actual(location, syntax_id, expr_ty);
    }
    pub(crate) fn record_unsealed_property_write(
        &mut self,
        table: TypeId,
        name: &str,
        value: TypeId,
    ) -> bool {
        let table = self.arena.follow(table);
        let record_table = match self.arena.get(table) {
            TypeKind::Table(table_type)
                if matches!(table_type.state, TableState::Free | TableState::Unsealed)
                    && !table_type.properties.contains_key(name) =>
            {
                table
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => {
                let base_table = *base_table;
                return self.record_unsealed_property_write(base_table, name, value);
            }
            TypeKind::Free(_) => table,
            _ => return false,
        };
        let writes = self
            .table_writes
            .unsealed_property_writes
            .entry(record_table)
            .or_default();
        if writes.contains_key(name) {
            return false;
        }
        writes.insert(name.to_owned(), value);
        true
    }
    pub(crate) fn report_known_non_table_property_write(
        &mut self,
        base_ty: TypeId,
        location: Option<Location>,
    ) -> bool {
        let base_ty = self.arena.follow(base_ty);
        if !matches!(
            self.arena.get(base_ty),
            TypeKind::Primitive(_) | TypeKind::Singleton(_)
        ) {
            return false;
        }
        let mut diagnostic = Diagnostic::type_mismatch("table", self.arena.summary(base_ty));
        diagnostic.primary_location = DiagnosticLocation::from_opt(location);
        self.generated.diagnostics.push(diagnostic);
        true
    }
    pub(crate) fn bind_index_expr(
        &mut self,
        locations: IndexExprLocations,
        syntax_id: SyntaxId,
        expr_ty: TypeId,
        base_ty: TypeId,
        index_ty: TypeId,
        eager_read: bool,
    ) {
        if self.is_never_type(base_ty) {
            self.bind_actual(locations.expr, syntax_id, expr_ty, self.primitives().never);
            return;
        }
        if self.is_error_type(base_ty) {
            self.bind_actual(locations.expr, syntax_id, expr_ty, self.primitives().error);
            return;
        }
        if self.is_dynamic(base_ty) {
            self.bind_actual(locations.expr, syntax_id, expr_ty, self.primitives().any);
            return;
        }
        if eager_read && let Some(read_ty) = self.index_expr_read_type(base_ty, index_ty) {
            self.bind_actual(locations.expr, syntax_id, expr_ty, read_ty);
        }
        if let Some(read_ty) = self.nonstrict_extern_dynamic_index_read_type(base_ty, index_ty) {
            self.bind_actual(locations.expr, syntax_id, expr_ty, read_ty);
            return;
        }
        self.generated.constraints.push(Constraint::read_indexer(
            base_ty,
            index_ty,
            expr_ty,
            locations.index,
        ));
        self.record_actual(locations.expr, syntax_id, expr_ty);
    }

    fn nonstrict_extern_dynamic_index_read_type(
        &self,
        base_ty: TypeId,
        index_ty: TypeId,
    ) -> Option<TypeId> {
        if self.input.mode != Mode::Nonstrict
            || member_access::string_singleton_key(self.arena, index_ty).is_some()
        {
            return None;
        }
        let TypeKind::Extern { indexer, .. } = self.arena.get(self.arena.follow(base_ty)) else {
            return None;
        };
        let indexer = indexer.clone();
        if let Some(indexer) = indexer
            && Subtyper::new(self.arena)
                .is_subtype(index_ty, indexer.key)
                .is_ok()
        {
            return Some(indexer.value);
        }
        let key = self.arena.follow(index_ty);
        match self.arena.get(key) {
            TypeKind::Primitive(PrimitiveType::String)
            | TypeKind::Primitive(PrimitiveType::Number)
            | TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Error => Some(self.primitives().any),
            _ => None,
        }
    }

    pub(crate) fn bind_index_expr_write(
        &mut self,
        location: Option<Location>,
        syntax_id: SyntaxId,
        expr_ty: TypeId,
        base_ty: TypeId,
        index_ty: TypeId,
    ) {
        if self.is_never_type(base_ty) {
            self.bind_actual(location, syntax_id, expr_ty, self.primitives().never);
            return;
        }
        if self.is_dynamic(base_ty) {
            self.bind_actual(location, syntax_id, expr_ty, self.primitives().any);
            return;
        }
        self.generated.constraints.push(Constraint::write_indexer(
            base_ty,
            index_ty,
            expr_ty,
            location.map(DiagnosticLocation::from),
        ));
        self.record_actual(location, syntax_id, expr_ty);
    }

    pub(crate) fn record_contextual_index_key_query(
        &mut self,
        base_ty: TypeId,
        index: &Expr,
        index_ty: TypeId,
    ) {
        let Some(query_ty) = self.contextual_index_key_query_type(base_ty, index_ty) else {
            return;
        };
        if self.arena.follow(query_ty) != self.arena.follow(index_ty) {
            self.record_actual(index.location(), index.syntax_id(), query_ty);
        }
    }

    fn contextual_index_key_query_type(&self, base_ty: TypeId, index_ty: TypeId) -> Option<TypeId> {
        if !matches!(
            self.arena.get(self.arena.follow(index_ty)),
            TypeKind::Singleton(_)
        ) {
            return None;
        }

        match self.arena.get(self.arena.follow(base_ty)) {
            TypeKind::Table(table) => {
                if let Some(name) = member_access::string_singleton_key(self.arena, index_ty)
                    && table.properties.contains_key(&name)
                {
                    return None;
                }
                let indexer = table.indexer.as_ref()?;
                Subtyper::new(self.arena)
                    .is_subtype(index_ty, indexer.key)
                    .is_ok()
                    .then_some(indexer.key)
            }
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                if let Some(name) = member_access::string_singleton_key(self.arena, index_ty)
                    && properties.contains_key(&name)
                {
                    return None;
                }
                let indexer = indexer.as_ref()?;
                Subtyper::new(self.arena)
                    .is_subtype(index_ty, indexer.key)
                    .is_ok()
                    .then_some(indexer.key)
            }
            _ => None,
        }
    }

    pub(crate) fn record_unsealed_indexer_write(
        &mut self,
        table: TypeId,
        key: TypeId,
        value: TypeId,
    ) -> bool {
        let table = self.arena.follow(table);
        let record_table = match self.arena.get(table).clone() {
            TypeKind::Table(table_type)
                if matches!(table_type.state, TableState::Free | TableState::Unsealed)
                    && table_type.indexer.is_none()
                    && self.arena.unsealed_indexer_key_needs_unknown_scope(key) =>
            {
                table
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => {
                return self.record_unsealed_indexer_write(base_table, key, value);
            }
            _ => return false,
        };
        let TypeKind::Table(mut table_type) = self.arena.get(record_table).clone() else {
            return false;
        };
        table_type.indexer = Some(TableIndexer {
            key: self.arena.scoped_unsealed_indexer_key(key),
            value,
            read_only: false,
        });
        self.arena
            .replace(record_table, TypeKind::Table(table_type));
        true
    }

    pub(crate) fn index_expr_read_type(
        &mut self,
        table_ty: TypeId,
        key_ty: TypeId,
    ) -> Option<TypeId> {
        let table_ty = self.arena.follow(table_ty);
        if let TypeKind::Table(table) = self.arena.get(table_ty) {
            if let Some(name) = member_access::string_singleton_key(self.arena, key_ty)
                && let Some(property) = table.properties.get(&name)
            {
                return Some(property.ty);
            }
            let state = table.state;
            let indexer = table.indexer.clone()?;
            if Subtyper::new(self.arena)
                .is_subtype(key_ty, indexer.key)
                .is_ok()
            {
                return Some(self.indexer_read_value(state, indexer.key, indexer.value));
            }
            return None;
        }
        match self.arena.get(table_ty).clone() {
            TypeKind::Table(_) => unreachable!("table handled without cloning above"),
            TypeKind::Union(types) => {
                let values = types
                    .into_iter()
                    .map(|ty| self.index_expr_read_type(ty, key_ty))
                    .collect::<Option<Vec<_>>>()?;
                let union = self.union_type(values);
                Some(simplify_type(self.arena, union))
            }
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                if let Some(name) = member_access::string_singleton_key(self.arena, key_ty)
                    && let Some(property) = properties.get(&name)
                {
                    return Some(property.ty);
                }
                let indexer = indexer?;
                Subtyper::new(self.arena)
                    .is_subtype(key_ty, indexer.key)
                    .is_ok()
                    .then_some(indexer.value)
            }
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {
                Some(self.primitives().any)
            }
            TypeKind::Primitive(PrimitiveType::String)
            | TypeKind::Singleton(SingletonType::String(_)) => {
                let name = member_access::string_singleton_key(self.arena, key_ty)?;
                string_primitive_property_type(self.arena, &name)
            }
            TypeKind::Primitive(PrimitiveType::Vector) => {
                let name = member_access::string_singleton_key(self.arena, key_ty)?;
                vector_primitive_property_type(self.arena, &name)
            }
            _ => None,
        }
    }

    fn indexer_read_value(&mut self, state: TableState, key: TypeId, value: TypeId) -> TypeId {
        if self.arena.unsealed_indexer_read_may_be_absent(state, key) {
            self.union_type(vec![value, self.primitives().nil])
        } else {
            value
        }
    }
    pub(crate) fn bind_actual(
        &mut self,
        location: Option<Location>,
        syntax_id: SyntaxId,
        expr_ty: TypeId,
        actual_ty: TypeId,
    ) {
        let unified = Unifier::new(self.arena).unify(expr_ty, actual_ty).is_ok();
        self.bind_free_to(expr_ty, actual_ty);
        if !unified {
            self.generated
                .constraints
                .push(Constraint::unify(expr_ty, actual_ty));
        }
        self.record_actual(location, syntax_id, actual_ty);
    }
    pub(crate) fn record_actual(
        &mut self,
        location: Option<Location>,
        syntax_id: SyntaxId,
        ty: TypeId,
    ) {
        self.generated
            .queries
            .record_actual(syntax_id, location.map(DiagnosticLocation::from), ty);
    }
    pub(crate) fn expect_type(
        &mut self,
        location: Option<Location>,
        actual_ty: TypeId,
        expected_ty: TypeId,
    ) {
        drop(Unifier::new(self.arena).unify(actual_ty, expected_ty));
        self.bind_free_to(actual_ty, expected_ty);
        self.generated
            .constraints
            .push(Constraint::unify(actual_ty, expected_ty));
        self.generated
            .constraints
            .push(Constraint::subtype(actual_ty, expected_ty, None));
        if let Some(location) = location {
            self.generated
                .queries
                .record_expected_location(DiagnosticLocation::from(location), expected_ty);
        }
    }

    pub(crate) fn bind_expected_type_without_constraints(
        &mut self,
        location: Option<Location>,
        actual_ty: TypeId,
        expected_ty: TypeId,
    ) {
        drop(Unifier::new(self.arena).unify(actual_ty, expected_ty));
        self.bind_free_to(actual_ty, expected_ty);
        if let Some(location) = location {
            self.generated
                .queries
                .record_expected_location(DiagnosticLocation::from(location), expected_ty);
        }
    }

    pub(crate) fn expr_type_discarding_call_results(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
    ) -> TypeId {
        self.expr_type_discarding_call_results_with(scope, expr, false)
    }
    pub(crate) fn expr_type_discarding_assignment_value(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
    ) -> TypeId {
        self.expr_type_discarding_call_results_with(scope, expr, true)
    }
    fn expr_type_discarding_call_results_with(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        infer_free_callee: bool,
    ) -> TypeId {
        let discard_call = matches!(expr, Expr::Call { .. });
        if discard_call {
            self.calls.discard_call_results.insert(expr.syntax_id());
            if infer_free_callee {
                self.calls
                    .infer_discarded_call_callees
                    .insert(expr.syntax_id());
            }
        }
        let ty = self.expr_type(scope, expr);
        if discard_call {
            self.calls.discard_call_results.remove(&expr.syntax_id());
            self.calls
                .infer_discarded_call_callees
                .remove(&expr.syntax_id());
        }
        ty
    }
    pub(crate) fn local_assignment_value_types(
        &mut self,
        scope: ScopeId,
        values: &[Expr],
        expected_types: &[Option<TypeId>],
    ) -> Vec<Option<(TypeId, bool, bool)>> {
        if let [value] = values
            && let Some(return_values) =
                self.call_return_values(scope, value, expected_types.len(), expected_types)
        {
            return return_values
                .into_iter()
                .map(|ty| ty.map(|ty| (ty, false, false)))
                .collect();
        }
        if let [value] = values
            && is_varargs_expr(value)
        {
            return (0..expected_types.len())
                .map(|index| self.vararg_type_at(index).map(|ty| (ty, false, false)))
                .collect();
        }

        let assigned = (0..expected_types.len())
            .map(|index| {
                let value = values.get(index)?;
                if index + 1 < values.len()
                    && is_varargs_expr(value)
                    && self
                        .function_frames
                        .vararg_stack
                        .last()
                        .and_then(|pack| *pack)
                        .is_none()
                {
                    return Some((self.primitives().nil, false, false));
                }
                let expected = expected_types[index];
                let expected_deferred = local_initializer_defers_expected_check(value)
                    || expected.is_some_and(|expected| {
                        self.local_initializer_defers_expected_for_type(value, expected)
                    });
                let expected = if expected_deferred { None } else { expected };
                let value_ty = self.expr_type_with_expected(scope, value, expected);
                Some((
                    value_ty,
                    matches!(value, Expr::Nil { .. }),
                    expected_deferred,
                ))
            })
            .collect::<Vec<_>>();
        for value in values.iter().skip(expected_types.len()) {
            self.expr_type_discarding_assignment_value(scope, value);
        }
        assigned
    }
    pub(crate) fn assignment_value_types(
        &mut self,
        scope: ScopeId,
        values: &[Expr],
        target_count: usize,
    ) -> Vec<AssignmentValue> {
        if let [value] = values
            && let Some(return_values) = self.call_return_values(scope, value, target_count, &[])
        {
            return return_values
                .into_iter()
                .map(|ty| AssignmentValue {
                    ty: ty.unwrap_or_else(|| self.primitives().nil),
                })
                .collect();
        }
        if let [value] = values
            && is_varargs_expr(value)
        {
            return (0..target_count)
                .map(|index| AssignmentValue {
                    ty: self
                        .vararg_type_at(index)
                        .unwrap_or_else(|| self.primitives().nil),
                })
                .collect();
        }

        let assigned = (0..target_count)
            .map(|index| {
                let Some(value) = values.get(index) else {
                    return AssignmentValue {
                        ty: self.primitives().nil,
                    };
                };
                if index + 1 < values.len()
                    && is_varargs_expr(value)
                    && self
                        .function_frames
                        .vararg_stack
                        .last()
                        .and_then(|pack| *pack)
                        .is_none()
                {
                    return AssignmentValue {
                        ty: self.primitives().nil,
                    };
                }
                AssignmentValue {
                    ty: self.expr_type(scope, value),
                }
            })
            .collect::<Vec<_>>();
        for value in values.iter().skip(target_count) {
            self.expr_type_discarding_assignment_value(scope, value);
        }
        assigned
    }

    fn local_initializer_defers_expected_for_type(&self, value: &Expr, expected: TypeId) -> bool {
        if !matches!(ungroup_expr(value), Expr::Table { .. }) {
            return false;
        }
        matches!(
            self.arena.get(self.arena.follow(expected)),
            TypeKind::Function(function) if is_top_function_type(self.arena, function)
        )
    }
    pub(crate) fn is_function_type(&self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        matches!(self.arena.get(ty), TypeKind::Function(_))
    }
    pub(crate) fn is_known_non_iterable_for_in_value(&self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        matches!(
            self.arena.get(ty),
            TypeKind::Primitive(_) | TypeKind::Singleton(_) | TypeKind::Extern { .. }
        )
    }
    pub(crate) fn is_metatable_type(&self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        matches!(self.arena.get(ty), TypeKind::Metatable { .. })
    }
    pub(crate) fn compound_assignment_result_type(
        &mut self,
        op: CompoundAssignOp,
        left: TypeId,
        right: TypeId,
        location: Option<Location>,
    ) -> (TypeId, bool) {
        let binary_op = compound_assign_binary_op(op);
        let result = self.placeholder_free_type("compound operator result");
        if self.push_binary_metamethod_call(binary_op, left, right, result, location) {
            return (result, true);
        }

        let primitives = self.primitives();
        match binary_op {
            BinaryOp::Concat => {
                if !self.is_dynamic(left) {
                    self.generated
                        .constraints
                        .push(Constraint::subtype_default_location(
                            left,
                            primitives.string,
                            location.map(DiagnosticLocation::from),
                        ));
                }
                if !self.is_dynamic(right) {
                    self.generated
                        .constraints
                        .push(Constraint::subtype_default_location(
                            right,
                            primitives.string,
                            location.map(DiagnosticLocation::from),
                        ));
                }
                (primitives.string, false)
            }
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::FloorDiv
            | BinaryOp::Mod
            | BinaryOp::Pow => {
                if !self.is_dynamic(left) {
                    self.generated
                        .constraints
                        .push(Constraint::subtype_default_location(
                            left,
                            primitives.number,
                            location.map(DiagnosticLocation::from),
                        ));
                }
                if !self.is_dynamic(right) {
                    self.generated
                        .constraints
                        .push(Constraint::subtype_default_location(
                            right,
                            primitives.number,
                            location.map(DiagnosticLocation::from),
                        ));
                }
                (primitives.number, false)
            }
            BinaryOp::CompareEq
            | BinaryOp::CompareNe
            | BinaryOp::CompareLt
            | BinaryOp::CompareLe
            | BinaryOp::CompareGt
            | BinaryOp::CompareGe
            | BinaryOp::And
            | BinaryOp::Or => unreachable!("compound assignment cannot use {binary_op:?}"),
        }
    }
    pub(crate) fn push_binary_metamethod_call(
        &mut self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
        result: TypeId,
        location: Option<Location>,
    ) -> bool {
        let Some((callee, arguments)) = self.binary_metamethod_call(op, left, right) else {
            return false;
        };
        let arguments = self.pack(arguments);
        let expected_returns = self.pack(vec![result]);
        self.generated.constraints.push(Constraint::call(
            callee,
            arguments,
            self.input.mode == Mode::Nonstrict,
            vec![
                location.map(DiagnosticLocation::from),
                location.map(DiagnosticLocation::from),
            ],
            Some(expected_returns),
            location.map(DiagnosticLocation::from),
            false,
        ));
        true
    }
    fn push_unary_metamethod_call(
        &mut self,
        metamethod: &str,
        operand: TypeId,
        result: TypeId,
        location: Option<Location>,
    ) -> bool {
        let Some(callee) = self.type_metamethod(operand, metamethod) else {
            return false;
        };
        let arguments = self.pack(vec![operand]);
        if crate::overload::resolve_call_for_constraint(
            self.arena,
            callee,
            arguments,
            true,
            self.input.mode == Mode::Nonstrict,
            false,
        )
        .is_err()
        {
            self.report_unary_operator_mismatch("-", operand, metamethod, location);
        }
        let expected_returns = self.pack(vec![result]);
        self.generated.constraints.push(Constraint::call(
            callee,
            arguments,
            self.input.mode == Mode::Nonstrict,
            vec![location.map(DiagnosticLocation::from)],
            Some(expected_returns),
            location.map(DiagnosticLocation::from),
            false,
        ));
        true
    }
    fn binary_metamethod_call(
        &self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
    ) -> Option<(TypeId, Vec<TypeId>)> {
        if is_relational_operator(op) {
            return self.relational_metamethod_call(op, left, right);
        }
        let metamethod = binary_metamethod_name(op)?;
        if let Some(callee) = self.type_metamethod(left, metamethod) {
            return Some((callee, vec![left, right]));
        }
        self.type_metamethod(right, metamethod)
            .map(|callee| (callee, vec![right, left]))
    }
    fn relational_metamethod_call(
        &self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
    ) -> Option<(TypeId, Vec<TypeId>)> {
        let metamethod = binary_metamethod_name(op)?;
        let left_metatable = self.arena.metatable_payload(left)?;
        let right_metatable = self.arena.metatable_payload(right)?;
        if self.arena.follow(left_metatable) != self.arena.follow(right_metatable) {
            return None;
        }
        self.metatable_property_type(left_metatable, metamethod)
            .map(|callee| (callee, vec![left, right]))
    }
    fn type_metamethod(&self, ty: TypeId, metamethod: &str) -> Option<TypeId> {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty) {
            TypeKind::Metatable { metatable, .. } => {
                self.metatable_property_type(*metatable, metamethod)
            }
            TypeKind::Extern { properties, .. } => {
                properties.get(metamethod).map(|property| property.ty)
            }
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {
                Some(self.primitives().any)
            }
            _ => None,
        }
    }
    fn metatable_property_type(&self, metatable: TypeId, property: &str) -> Option<TypeId> {
        let metatable = self.arena.follow(metatable);
        if let Some(property) = self.unsealed_property_write(metatable, property) {
            return Some(property);
        }
        match self.arena.get(metatable) {
            TypeKind::Table(table) => table.properties.get(property).map(|property| property.ty),
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {
                Some(self.primitives().any)
            }
            _ => None,
        }
    }
    pub(crate) fn vector_arithmetic_result(
        &self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
    ) -> Option<TypeId> {
        let vector = self.primitives().vector;
        let left_vector = self.is_vector_like(left);
        let right_vector = self.is_vector_like(right);
        match op {
            BinaryOp::Add | BinaryOp::Sub if left_vector && right_vector => Some(vector),
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::FloorDiv
                if (left_vector && (right_vector || self.is_number_like(right)))
                    || (right_vector && self.is_number_like(left)) =>
            {
                Some(vector)
            }
            _ => None,
        }
    }
    pub(crate) fn is_vector_like(&self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        matches!(
            self.arena.get(ty),
            TypeKind::Primitive(PrimitiveType::Vector)
        )
    }
    pub(crate) fn is_number_like(&self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        matches!(
            self.arena.get(ty),
            TypeKind::Primitive(PrimitiveType::Number)
        )
    }
    fn expected_add_type_function_result(
        &self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
        expected: Option<TypeId>,
    ) -> Option<TypeId> {
        if op != BinaryOp::Add {
            return None;
        }
        let expected = self.arena.follow(expected?);
        let TypeKind::TypeFunctionInstance { name, arguments } = self.arena.get(expected) else {
            return None;
        };
        let (name, arguments) = (name.clone(), arguments.clone());
        if name != "add"
            || TypeFunctionRuntime::new().reduce(self.arena, &name, &arguments)
                != Reduction::Pending
            || !self.add_type_function_arguments_match(&arguments, left, right)
        {
            return None;
        }
        Some(expected)
    }
    fn add_type_function_arguments_match(
        &self,
        arguments: &[TypeId],
        left: TypeId,
        right: TypeId,
    ) -> bool {
        match arguments {
            [single] => {
                self.type_may_flow_to_add_operand(left, *single)
                    && self.type_may_flow_to_add_operand(right, *single)
            }
            [expected_left, expected_right] => {
                self.type_may_flow_to_add_operand(left, *expected_left)
                    && self.type_may_flow_to_add_operand(right, *expected_right)
            }
            _ => false,
        }
    }
    fn type_may_flow_to_add_operand(&self, actual: TypeId, expected: TypeId) -> bool {
        Subtyper::new(self.arena)
            .is_subtype(actual, expected)
            .is_ok()
            && Subtyper::new(self.arena)
                .is_subtype(expected, actual)
                .is_ok()
    }
    pub(crate) fn callable_type(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        fallback: TypeId,
    ) -> TypeId {
        match expr {
            Expr::Local { local, .. } => self
                .input
                .dfg
                .local(local.id)
                .map(|def| self.input.dfg.get(def).ty)
                .unwrap_or(fallback),
            Expr::IndexName { expr, index, .. } => {
                let base = self
                    .generated
                    .queries
                    .actual_by_syntax(expr.syntax_id())
                    .unwrap_or_else(|| self.dfg_type_for_expr(expr));
                self.callable_property_type(base, index.as_str())
                    .unwrap_or(fallback)
            }
            Expr::IndexExpr { expr, index, .. } => self
                .string_property_index(index)
                .and_then(|property| {
                    let base = self
                        .generated
                        .queries
                        .actual_by_syntax(expr.syntax_id())
                        .unwrap_or_else(|| self.dfg_type_for_expr(expr));
                    self.callable_property_type(base, &property)
                })
                .unwrap_or(fallback),
            Expr::Call { .. } => self
                .call_expression_callable_type(scope, expr)
                .unwrap_or(fallback),
            _ => fallback,
        }
    }

    fn callable_property_type(&mut self, ty: TypeId, property: &str) -> Option<TypeId> {
        self.property_type(ty, property)
            .or_else(|| self.nilable_union_callable_property_type(ty, property))
    }

    fn nilable_union_callable_property_type(
        &mut self,
        ty: TypeId,
        property: &str,
    ) -> Option<TypeId> {
        let ty = self.arena.follow(ty);
        let TypeKind::Union(options) = self.arena.get(ty) else {
            return None;
        };
        let options = options.clone();
        let mut property_types = Vec::new();
        let mut saw_nil = false;
        for option in options {
            let option = self.arena.follow(option);
            if self.arena.is_nil(option) {
                saw_nil = true;
                continue;
            }
            let property_ty = self.property_type(option, property)?;
            property_types.push(property_ty);
        }
        if !saw_nil || property_types.is_empty() {
            return None;
        }
        Some(self.union_type(property_types))
    }

    fn call_expression_callable_type(&mut self, scope: ScopeId, expr: &Expr) -> Option<TypeId> {
        let Expr::Call {
            func,
            args,
            type_arguments,
            is_self,
            ..
        } = expr
        else {
            return None;
        };
        if !type_arguments.is_empty() {
            return None;
        }

        if matches!(func.as_ref(), Expr::Global { name, .. } if matches!(name.as_str(), "ipairs" | "next" | "pairs"))
            && let Some(return_values) = self.call_return_values(scope, expr, 1, &[])
            && let Some(Some(first)) = return_values.first().copied()
        {
            return Some(first);
        }

        let callee = self.dfg_type_for_expr(func);
        let expected_callee = self.callable_type(scope, func, callee);
        let (instantiated_callee, instantiated_generic) =
            self.instantiate_expected_call_callee(expected_callee);
        if !instantiated_generic {
            return None;
        }

        let receiver = if *is_self && matches!(func.as_ref(), Expr::IndexName { .. }) {
            ReceiverParameter::Supplied
        } else {
            ReceiverParameter::Explicit
        };
        let expected_parameters =
            ExpectedCallParameterPack::from_callee(self.arena, instantiated_callee, receiver);
        for (index, arg) in args.iter().enumerate() {
            let Some(expected) = expected_parameters.parameter_at(self.arena, index) else {
                continue;
            };
            let actual = self
                .generated
                .queries
                .actual_by_syntax(arg.syntax_id())
                .unwrap_or_else(|| self.dfg_type_for_expr(arg));
            self.bind_call_expression_callee_parameter(actual, expected);
        }

        self.function_result_type(instantiated_callee)
    }

    fn bind_call_expression_callee_parameter(&mut self, actual: TypeId, expected: TypeId) {
        let expected = self.arena.follow(expected);
        if !matches!(self.arena.get(expected), TypeKind::Free(_)) {
            return;
        }
        let actual = self.arena.follow(actual);
        if matches!(
            self.arena.get(actual),
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Free(_)
        ) {
            return;
        }
        self.bind_free_to(expected, actual);
    }

    pub(crate) fn callee_is_top_function_refinement(&self, expr: &Expr, callee: TypeId) -> bool {
        if !self.type_contains_top_function(callee) {
            return false;
        }
        let local_id = match expr {
            Expr::Local { local, .. } => Some(local.id),
            Expr::IndexName { expr, .. } | Expr::IndexExpr { expr, .. } => {
                self.local_from_grouped_expr(expr)
            }
            Expr::Group { expr, .. } => {
                return self.callee_is_top_function_refinement(expr, callee);
            }
            _ => None,
        };
        local_id.is_some_and(|local_id| {
            self.refined_type(&RefinementKey::Symbol(Symbol::Local(local_id)))
                .is_some()
        })
    }

    fn type_contains_top_function(&self, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Function(function) => is_top_function_type(self.arena, function),
            TypeKind::Union(options) => options
                .iter()
                .any(|option| self.type_contains_top_function(*option)),
            _ => false,
        }
    }

    pub(crate) fn report_top_function_refinement_call(&mut self, location: Option<Location>) {
        let diagnostic = Diagnostic::error(
            DiagnosticCategory::Call,
            DiagnosticLocation::from_opt(location),
        )
        .with_typed(Payload::NotCallable);
        self.generated.diagnostics.push(diagnostic);
    }

    pub(crate) fn property_type(&mut self, ty: TypeId, property: &str) -> Option<TypeId> {
        self.property_type_with_seen(ty, property, &mut Vec::new())
    }

    fn property_type_with_seen(
        &mut self,
        ty: TypeId,
        property: &str,
        seen: &mut Vec<TypeId>,
    ) -> Option<TypeId> {
        let ty = self.arena.follow(ty);
        if seen.contains(&ty) {
            return None;
        }
        seen.push(ty);
        if let Some(ty) = self.unsealed_property_write(ty, property) {
            return Some(ty);
        }
        if let TypeKind::Table(table) = self.arena.get(ty) {
            let direct = table.properties.get(property).and_then(|property| {
                if property.write_only
                    && !matches!(table.state, TableState::Unsealed | TableState::Free)
                {
                    None
                } else {
                    Some(property.ty)
                }
            });
            if direct.is_some() {
                return direct;
            }
            let indexer = table.indexer.clone()?;
            let key = self.arena.alloc(TypeKind::Singleton(SingletonType::String(
                property.to_owned(),
            )));
            return Subtyper::new(self.arena)
                .is_subtype(key, indexer.key)
                .is_ok()
                .then_some(indexer.value);
        }
        match self.arena.get(ty).clone() {
            TypeKind::Table(_) => unreachable!("table handled without cloning above"),
            TypeKind::Metatable {
                table: base_table,
                metatable,
                ..
            } => {
                if let Some(property) = self.arena.direct_read_property(base_table, property) {
                    return Some(property);
                }
                if self.is_dynamic(metatable) {
                    return Some(self.primitives().any);
                }
                let index = self
                    .arena
                    .direct_read_property(metatable, "__index")
                    .or_else(|| self.unsealed_property_write(metatable, "__index"))?;
                let index = self.arena.follow(index);
                if matches!(self.arena.get(index), TypeKind::Function(_)) {
                    return Some(
                        self.index_function_result_type(index)
                            .unwrap_or_else(|| self.primitives().any),
                    );
                }
                self.property_type_with_seen(index, property, seen)
            }
            TypeKind::Union(types) => {
                if self.arena.is_string_like(ty) {
                    return string_primitive_property_type(self.arena, property);
                }
                let properties = types
                    .into_iter()
                    .map(|ty| self.property_type_with_seen(ty, property, seen))
                    .collect::<Option<Vec<_>>>()?;
                if properties
                    .iter()
                    .any(|property| self.arena.follow(*property) == self.primitives().any)
                {
                    return Some(self.primitives().any);
                }
                Some(self.union_type(properties))
            }
            TypeKind::Intersection(types) => {
                let properties = types
                    .into_iter()
                    .filter_map(|ty| self.property_type_with_seen(ty, property, seen))
                    .collect::<Vec<_>>();
                (!properties.is_empty()).then(|| self.intersection_type(properties))
            }
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => properties
                .get(property)
                .and_then(|property| (!property.write_only).then_some(property.ty))
                .or_else(|| {
                    let indexer = indexer?;
                    let key = self.arena.alloc(TypeKind::Singleton(SingletonType::String(
                        property.to_owned(),
                    )));
                    Subtyper::new(self.arena)
                        .is_subtype(key, indexer.key)
                        .is_ok()
                        .then_some(indexer.value)
                }),
            TypeKind::Negation(inner) if self.negated_type_may_have_properties(inner) => {
                Some(self.primitives().any)
            }
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {
                Some(self.primitives().any)
            }
            TypeKind::Primitive(PrimitiveType::String)
            | TypeKind::Singleton(SingletonType::String(_)) => {
                string_primitive_property_type(self.arena, property)
            }
            TypeKind::Primitive(PrimitiveType::Vector) => {
                vector_primitive_property_type(self.arena, property)
            }
            _ => None,
        }
    }
    fn negated_type_may_have_properties(&self, inner: TypeId) -> bool {
        !matches!(
            self.arena.get(self.arena.follow(inner)),
            TypeKind::Table(_) | TypeKind::Metatable { .. }
        )
    }
    fn unsealed_property_write(&self, table: TypeId, property: &str) -> Option<TypeId> {
        self.table_writes
            .unsealed_property_writes
            .get(&self.arena.follow(table))?
            .get(property)
            .copied()
    }
    fn index_function_result_type(&self, index: TypeId) -> Option<TypeId> {
        self.function_result_type(index).or_else(|| {
            (self.function_fixed_return_count(index) == Some(0)).then_some(self.primitives().nil)
        })
    }
    pub(crate) fn truthiness(&self, ty: TypeId) -> Truthiness {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Primitive(PrimitiveType::Nil)
            | TypeKind::Singleton(SingletonType::Boolean(false)) => Truthiness::AlwaysFalsy,
            TypeKind::Primitive(PrimitiveType::Boolean)
            | TypeKind::Singleton(SingletonType::Boolean(true))
            | TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Error
            | TypeKind::Blocked(_)
            | TypeKind::Free(_) => Truthiness::Unknown,
            TypeKind::Union(types) => {
                let mut saw_truthy = false;
                let mut saw_falsy = false;
                for ty in types {
                    match self.truthiness(*ty) {
                        Truthiness::AlwaysTruthy => saw_truthy = true,
                        Truthiness::AlwaysFalsy => saw_falsy = true,
                        Truthiness::Unknown => return Truthiness::Unknown,
                    }
                }
                match (saw_truthy, saw_falsy) {
                    (true, false) => Truthiness::AlwaysTruthy,
                    (false, true) => Truthiness::AlwaysFalsy,
                    _ => Truthiness::Unknown,
                }
            }
            _ => Truthiness::AlwaysTruthy,
        }
    }
    pub(crate) fn check_equality_operands(
        &mut self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
        location: Option<Location>,
    ) {
        if self.equality_operands_may_overlap(left, right) {
            return;
        }
        if self.arena.is_nil(left) || self.arena.is_nil(right) {
            return;
        }

        let mut diagnostic = Diagnostic::binary_operator_error(
            equality_operator_text(op),
            self.arena.summary(left),
            self.arena.summary(right),
            "equality",
        );
        diagnostic.primary_location = DiagnosticLocation::from_opt(location);
        self.generated.diagnostics.push(diagnostic);
    }
    pub(crate) fn equality_operands_may_overlap(&self, left: TypeId, right: TypeId) -> bool {
        let left = self.arena.follow(left);
        let right = self.arena.follow(right);
        if left == right {
            return true;
        }

        match (self.arena.get(left), self.arena.get(right)) {
            (TypeKind::Never, _) | (_, TypeKind::Never) => false,
            (TypeKind::Union(lefts), _) => lefts
                .iter()
                .any(|left| self.equality_operands_may_overlap(*left, right)),
            (_, TypeKind::Union(rights)) => rights
                .iter()
                .any(|right| self.equality_operands_may_overlap(left, *right)),
            (
                TypeKind::Any
                | TypeKind::Unknown
                | TypeKind::Error
                | TypeKind::Blocked(_)
                | TypeKind::Free(_)
                | TypeKind::Generic(_),
                _,
            )
            | (
                _,
                TypeKind::Any
                | TypeKind::Unknown
                | TypeKind::Error
                | TypeKind::Blocked(_)
                | TypeKind::Free(_)
                | TypeKind::Generic(_),
            ) => true,
            (TypeKind::Primitive(left), TypeKind::Primitive(right)) => left == right,
            (TypeKind::Primitive(left), TypeKind::Singleton(right)) => *left == right.primitive(),
            (TypeKind::Singleton(left), TypeKind::Primitive(right)) => left.primitive() == *right,
            (TypeKind::Singleton(left), TypeKind::Singleton(right)) => {
                left.primitive() == right.primitive()
            }
            (
                TypeKind::Extern {
                    name: left,
                    parents: left_parents,
                    ..
                },
                TypeKind::Extern {
                    name: right,
                    parents: right_parents,
                    ..
                },
            ) => {
                extern_is_subtype(left, left_parents, right)
                    || extern_is_subtype(right, right_parents, left)
            }
            (
                TypeKind::Metatable {
                    metatable: left_metatable,
                    ..
                },
                TypeKind::Metatable {
                    metatable: right_metatable,
                    ..
                },
            ) => self.arena.follow(*left_metatable) == self.arena.follow(*right_metatable),
            (TypeKind::Metatable { .. }, TypeKind::Table(_))
            | (TypeKind::Table(_), TypeKind::Metatable { .. }) => false,
            (
                TypeKind::Primitive(_) | TypeKind::Singleton(_),
                TypeKind::Table(_)
                | TypeKind::Function(_)
                | TypeKind::Extern { .. }
                | TypeKind::Metatable { .. }
                | TypeKind::TypeFunctionInstance { .. },
            )
            | (
                TypeKind::Table(_)
                | TypeKind::Function(_)
                | TypeKind::Extern { .. }
                | TypeKind::Metatable { .. }
                | TypeKind::TypeFunctionInstance { .. },
                TypeKind::Primitive(_) | TypeKind::Singleton(_),
            ) => false,
            _ => true,
        }
    }
    pub(crate) fn check_relational_operands(
        &mut self,
        op: BinaryOp,
        location: Option<Location>,
        left: TypeId,
        right: TypeId,
        property_free_operands: bool,
    ) {
        if self.bind_free_relational_operand_to_refined_uninhabited_counterpart(left, right) {
            return;
        }
        if self.type_is_uninhabited(left) || self.type_is_uninhabited(right) {
            return;
        }
        let diagnostic_location = DiagnosticLocation::from_opt(location);
        let (left, right) = if self.input.mode == Mode::Nonstrict {
            (self.strip_nil(left), self.strip_nil(right))
        } else {
            (left, right)
        };
        let left_kind = self.relational_operand_kind(left);
        let right_kind = self.relational_operand_kind(right);
        if matches!(left_kind, RelationalOperandKind::Unknown)
            || matches!(right_kind, RelationalOperandKind::Unknown)
        {
            return;
        }
        // Nonstrict mode tolerates unresolved property types on both sides,
        // matching Luau's permissive treatment of unannotated parameters.
        if matches!(
            (left_kind, right_kind),
            (RelationalOperandKind::Free, RelationalOperandKind::Free)
        ) && property_free_operands
            && self.input.mode != Mode::Nonstrict
        {
            let mut diagnostic = Diagnostic::binary_operator_error(
                relational_operator_text(op),
                self.arena.summary(left),
                self.arena.summary(right),
                "relational",
            );
            diagnostic.primary_location = diagnostic_location;
            self.generated.diagnostics.push(diagnostic);
            return;
        }
        // `a < b` requires both operands to be the same orderable type, so a free
        // operand compared against a concrete `number`/`string` must be that type.
        // In nonstrict mode parameters are inferred from such usage, so bind the
        // free operand rather than leaving it unconstrained.
        if self.input.mode == Mode::Nonstrict {
            if let Some(orderable) = self.relational_orderable_primitive(right_kind)
                && matches!(left_kind, RelationalOperandKind::Free)
            {
                self.bind_free_to(left, orderable);
                return;
            }
            if let Some(orderable) = self.relational_orderable_primitive(left_kind)
                && matches!(right_kind, RelationalOperandKind::Free)
            {
                self.bind_free_to(right, orderable);
                return;
            }
        }
        if matches!(left_kind, RelationalOperandKind::Free)
            || matches!(right_kind, RelationalOperandKind::Free)
        {
            return;
        }
        if left_kind == right_kind
            && matches!(
                left_kind,
                RelationalOperandKind::Number | RelationalOperandKind::String
            )
        {
            return;
        }
        if self.relational_operand_orderable_union(left)
            == self.relational_operand_orderable_union(right)
            && self.relational_operand_orderable_union(left).is_some()
        {
            return;
        }

        let mut diagnostic = self.relational_operator_error(op, left, right);
        diagnostic.primary_location = diagnostic_location;
        self.generated.diagnostics.push(diagnostic);
    }

    fn bind_free_relational_operand_to_refined_uninhabited_counterpart(
        &mut self,
        left: TypeId,
        right: TypeId,
    ) -> bool {
        if self.type_is_refined_uninhabited(left) && self.relational_operand_is_free(right) {
            self.bind_free_to(right, left);
            return true;
        }
        if self.type_is_refined_uninhabited(right) && self.relational_operand_is_free(left) {
            self.bind_free_to(left, right);
            return true;
        }
        false
    }

    fn relational_operand_is_free(&self, ty: TypeId) -> bool {
        matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Free(_))
    }

    fn relational_operator_error(&self, op: BinaryOp, left: TypeId, right: TypeId) -> Diagnostic {
        let left_summary = self.arena.summary(left);
        let right_summary = self.arena.summary(right);
        let mut diagnostic = Diagnostic::binary_operator_error(
            relational_operator_text(op),
            left_summary.clone(),
            right_summary.clone(),
            "relational",
        );
        if self.relational_metatables_differ(left, right) {
            diagnostic.context = Some(format!(
                "Types {left_summary} and {right_summary} cannot be compared with {} because they do not have the same metatable",
                relational_operator_text(op)
            ));
            let mut typed = std::mem::take(&mut diagnostic.typed_payload);
            if let Payload::BinaryOperatorMismatch {
                metatable_mismatch, ..
            } = &mut typed
            {
                *metatable_mismatch = true;
            }
            diagnostic.set_typed(typed);
        }
        diagnostic
    }

    fn relational_metatables_differ(&self, left: TypeId, right: TypeId) -> bool {
        let left_metatable = self.arena.metatable_payload(left);
        let right_metatable = self.arena.metatable_payload(right);
        (left_metatable.is_some() || right_metatable.is_some()) && left_metatable != right_metatable
    }

    /// The primitive a free operand must take to be ordered against an operand
    /// of `kind`, if `kind` is itself a concrete orderable primitive.
    fn relational_orderable_primitive(&self, kind: RelationalOperandKind) -> Option<TypeId> {
        match kind {
            RelationalOperandKind::Number => Some(self.primitives().number),
            RelationalOperandKind::String => Some(self.primitives().string),
            _ => None,
        }
    }
    pub(crate) fn relational_operand_kind(&self, ty: TypeId) -> RelationalOperandKind {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Primitive(PrimitiveType::Number) => RelationalOperandKind::Number,
            TypeKind::Primitive(PrimitiveType::String)
            | TypeKind::Singleton(SingletonType::String(_)) => RelationalOperandKind::String,
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {
                RelationalOperandKind::Unknown
            }
            TypeKind::Free(_) => RelationalOperandKind::Free,
            TypeKind::Union(types) => {
                let mut kind = None;
                for ty in types {
                    let option = self.relational_operand_kind(*ty);
                    if matches!(option, RelationalOperandKind::Unknown) {
                        return RelationalOperandKind::Unknown;
                    }
                    match kind {
                        None => kind = Some(option),
                        Some(existing) if existing == option => {}
                        Some(_) => return RelationalOperandKind::Invalid,
                    }
                }
                kind.unwrap_or(RelationalOperandKind::Invalid)
            }
            TypeKind::Intersection(types) => self.relational_intersection_operand_kind(types),
            _ => RelationalOperandKind::Invalid,
        }
    }
    fn relational_intersection_operand_kind(&self, types: &[TypeId]) -> RelationalOperandKind {
        let mut concrete = None;
        let mut indeterminate = false;
        for ty in types {
            match self.relational_operand_kind(*ty) {
                RelationalOperandKind::Number | RelationalOperandKind::String => {
                    let kind = self.relational_operand_kind(*ty);
                    match concrete {
                        None => concrete = Some(kind),
                        Some(existing) if existing == kind => {}
                        Some(_) => return RelationalOperandKind::Invalid,
                    }
                }
                RelationalOperandKind::Free | RelationalOperandKind::Unknown => {
                    indeterminate = true;
                }
                RelationalOperandKind::Invalid => {
                    if !matches!(
                        self.arena.get(self.arena.follow(*ty)),
                        TypeKind::Negation(_) | TypeKind::Generic(_)
                    ) {
                        return RelationalOperandKind::Invalid;
                    }
                    indeterminate = true;
                }
            }
        }
        concrete.unwrap_or(if indeterminate {
            RelationalOperandKind::Free
        } else {
            RelationalOperandKind::Invalid
        })
    }
    fn relational_operand_orderable_union(&self, ty: TypeId) -> Option<BTreeSet<PrimitiveType>> {
        let TypeKind::Union(types) = self.arena.get(self.arena.follow(ty)) else {
            return None;
        };
        let mut primitives = BTreeSet::new();
        for ty in types {
            match self.relational_operand_kind(*ty) {
                RelationalOperandKind::Number => {
                    primitives.insert(PrimitiveType::Number);
                }
                RelationalOperandKind::String => {
                    primitives.insert(PrimitiveType::String);
                }
                _ => return None,
            }
        }
        (primitives.len() > 1).then_some(primitives)
    }
    pub(crate) fn logical_result_part(&mut self, ty: TypeId) -> TypeId {
        let ty = self.arena.follow(ty);
        let union_types = match self.arena.get(ty) {
            TypeKind::Singleton(SingletonType::String(_)) => return self.primitives().string,
            TypeKind::Union(types) => types.clone(),
            _ => return ty,
        };
        let widened = union_types
            .into_iter()
            .map(|ty| self.logical_result_part(ty))
            .collect::<Vec<_>>();
        self.union_type(widened)
    }
    pub(crate) fn logical_result_part_with_expected(
        &mut self,
        ty: TypeId,
        expected: Option<TypeId>,
    ) -> TypeId {
        let ty = self.arena.follow(ty);
        if let Some(expected) = expected
            && self.expected_accepts_logical_literal(ty, expected)
        {
            return ty;
        }
        self.logical_result_part(ty)
    }
    pub(crate) fn expected_accepts_logical_literal(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> bool {
        let actual = self.arena.follow(actual);
        let expected = self.arena.follow(expected);
        match (self.arena.get(actual), self.arena.get(expected)) {
            (
                TypeKind::Singleton(SingletonType::String(actual)),
                TypeKind::Singleton(SingletonType::String(expected)),
            ) => actual == expected,
            (
                TypeKind::Singleton(SingletonType::String(_)),
                TypeKind::Primitive(PrimitiveType::String),
            ) => true,
            (TypeKind::Singleton(_), TypeKind::Any | TypeKind::Unknown | TypeKind::Error) => true,
            (_, TypeKind::Union(options)) => options
                .iter()
                .any(|option| self.expected_accepts_logical_literal(actual, *option)),
            _ => false,
        }
    }
    pub(crate) fn is_table_cast(&self, actual: TypeId, annotation: TypeId) -> bool {
        self.is_optional_table_like(actual) && self.is_optional_table_like(annotation)
    }

    fn is_optional_table_like(&self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty) {
            TypeKind::Table(_) => true,
            TypeKind::Union(options) => {
                let mut saw_table = false;
                for option in options {
                    match self.arena.get(self.arena.follow(*option)) {
                        TypeKind::Primitive(PrimitiveType::Nil) => {}
                        TypeKind::Table(_) => saw_table = true,
                        _ => return false,
                    }
                }
                saw_table
            }
            _ => false,
        }
    }
    pub(crate) fn type_assertion_needs_error(&self, actual: TypeId, annotation: TypeId) -> bool {
        if self.contains_dynamic_type(actual, &mut BTreeSet::new())
            || self.contains_dynamic_type(annotation, &mut BTreeSet::new())
            || matches!(
                (
                    self.arena.get(self.arena.follow(actual)),
                    self.arena.get(self.arena.follow(annotation)),
                ),
                (TypeKind::Never, _) | (_, TypeKind::Never)
            )
            || self.is_table_cast(actual, annotation)
        {
            return false;
        }

        Subtyper::new(self.arena)
            .is_subtype(actual, annotation)
            .is_err()
            && Subtyper::new(self.arena)
                .is_subtype(annotation, actual)
                .is_err()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Truthiness {
    AlwaysTruthy,
    AlwaysFalsy,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefinementSense {
    Truthy,
    Falsy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypeofRefinementSense {
    Is,
    IsNot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypeofTag {
    Primitive(PrimitiveType),
    Function,
    Table,
    Userdata,
    Extern(String),
}

pub fn is_operator_metamethod_name(name: &str) -> bool {
    matches!(
        name,
        "__add"
            | "__sub"
            | "__mul"
            | "__div"
            | "__idiv"
            | "__mod"
            | "__pow"
            | "__concat"
            | "__unm"
            | "__lt"
            | "__le"
            | "__eq"
    )
}

pub fn compound_assign_binary_op(op: CompoundAssignOp) -> BinaryOp {
    match op {
        CompoundAssignOp::Add => BinaryOp::Add,
        CompoundAssignOp::Sub => BinaryOp::Sub,
        CompoundAssignOp::Mul => BinaryOp::Mul,
        CompoundAssignOp::Div => BinaryOp::Div,
        CompoundAssignOp::FloorDiv => BinaryOp::FloorDiv,
        CompoundAssignOp::Mod => BinaryOp::Mod,
        CompoundAssignOp::Pow => BinaryOp::Pow,
        CompoundAssignOp::Concat => BinaryOp::Concat,
    }
}

#[derive(Clone, Copy)]
pub struct ExpectedTableItem {
    pub(crate) ty: TypeId,
    pub(crate) write_only: bool,
}

pub fn expected_table_item(table: &TableType, item: &TableItem) -> Option<ExpectedTableItem> {
    match (&item.kind, &item.key) {
        (TableItemKind::Record, Some(Expr::String { value, .. })) => {
            expected_named_table_item(table, value)
        }
        (TableItemKind::Record, Some(Expr::Global { name, .. })) => {
            expected_named_table_item(table, name.as_str())
        }
        (TableItemKind::General, Some(Expr::String { value, .. })) => {
            expected_named_table_item(table, value)
        }
        (TableItemKind::Item, _) | (TableItemKind::General, Some(_)) => {
            expected_indexed_table_item(table)
        }
        _ => None,
    }
}

fn expected_named_table_item(table: &TableType, name: &str) -> Option<ExpectedTableItem> {
    table
        .properties
        .get(name)
        .map(|property| ExpectedTableItem {
            ty: property.ty,
            write_only: property.write_only,
        })
        .or_else(|| expected_indexed_table_item(table))
}

fn expected_indexed_table_item(table: &TableType) -> Option<ExpectedTableItem> {
    table.indexer.as_ref().map(|indexer| ExpectedTableItem {
        ty: indexer.value,
        write_only: false,
    })
}

pub fn shadowed_table_item_indices(items: &[TableItem]) -> BTreeSet<usize> {
    let mut last_index_by_key = BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        if let Some(key) = static_table_item_key(item) {
            last_index_by_key.insert(key, index);
        }
    }

    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let key = static_table_item_key(item)?;
            (last_index_by_key.get(&key) != Some(&index)).then_some(index)
        })
        .collect()
}

pub fn static_table_item_key(item: &TableItem) -> Option<String> {
    match (&item.kind, &item.key) {
        (TableItemKind::Record, Some(Expr::String { value, .. }))
        | (TableItemKind::General, Some(Expr::String { value, .. })) => Some(value.clone()),
        (TableItemKind::Record, Some(Expr::Global { name, .. })) => Some(name.as_str().to_owned()),
        _ => None,
    }
}

pub fn merge_expected_table(arena: &Arena, into: &mut TableType, table: TableType) -> Option<()> {
    for (name, property) in table.properties {
        if let Some(existing) = into.properties.get(&name) {
            if existing.read_only != property.read_only
                || existing.write_only != property.write_only
                || existing.deprecated != property.deprecated
                || arena.follow(existing.ty) != arena.follow(property.ty)
                || match (existing.write_ty, property.write_ty) {
                    (Some(existing), Some(property)) => {
                        arena.follow(existing) != arena.follow(property)
                    }
                    (None, None) => false,
                    _ => true,
                }
            {
                return None;
            }
        } else {
            into.properties.insert(name, property);
        }
    }

    if let Some(indexer) = table.indexer {
        match &into.indexer {
            Some(existing) if existing != &indexer => return None,
            Some(_) => {}
            None => into.indexer = Some(indexer),
        }
    }

    Some(())
}

pub fn widened_table_literal_value_type(arena: &Arena, expr: &Expr) -> Option<TypeId> {
    match expr {
        Expr::Nil { .. } => Some(arena.primitives().nil),
        Expr::Bool { .. } => Some(arena.primitives().boolean),
        Expr::Number { .. } | Expr::Integer { .. } => Some(arena.primitives().number),
        Expr::String { .. } => Some(arena.primitives().string),
        _ => None,
    }
}

pub fn is_varargs_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Varargs { .. } => true,
        Expr::Group { expr, .. } => is_varargs_expr(expr),
        _ => false,
    }
}

pub fn expr_is_table_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Table { .. } => true,
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => expr_is_table_literal(expr),
        _ => false,
    }
}

/// Returns the method name when `func` accesses a property of the `string`
/// library global (e.g. `string.find`).
pub fn string_lib_method(func: &Expr) -> Option<&str> {
    let Expr::IndexName { expr, index, .. } = ungroup_expr(func) else {
        return None;
    };
    let Expr::Global { name, .. } = ungroup_expr(expr) else {
        return None;
    };
    (name.as_str() == "string").then(|| index.as_str())
}

pub fn string_literal(expr: &Expr) -> Option<&str> {
    match ungroup_expr(expr) {
        Expr::String { value, .. } => Some(value.as_str()),
        _ => None,
    }
}

/// Counts the captures in a Lua string pattern, returning one entry per capture
/// (`true` for a position capture `()` which yields a number, `false` for a
/// value capture which yields a string). Returns `None` for malformed patterns
/// (unbalanced captures or sets), so callers fall back to default typing.
pub fn lua_pattern_captures(pattern: &str) -> Option<Vec<bool>> {
    let bytes = pattern.as_bytes();
    let len = bytes.len();
    let mut index = 0;
    let mut open_captures = 0i32;
    let mut captures = Vec::new();
    while index < len {
        match bytes[index] {
            b'%' => {
                if index + 1 >= len {
                    return None;
                }
                if bytes[index + 1] == b'b' {
                    // `%bxy` matches a balanced pair; the two delimiter bytes are
                    // literal, not captures.
                    if index + 3 >= len {
                        return None;
                    }
                    index += 4;
                } else {
                    index += 2;
                }
            }
            b'[' => {
                index += 1;
                if index < len && bytes[index] == b'^' {
                    index += 1;
                }
                // A `]` immediately after `[` (or `[^`) is a literal set member.
                if index < len && bytes[index] == b']' {
                    index += 1;
                }
                while index < len && bytes[index] != b']' {
                    if bytes[index] == b'%' {
                        index += 1;
                    }
                    index += 1;
                }
                if index >= len {
                    return None;
                }
                index += 1;
            }
            b'(' => {
                if index + 1 < len && bytes[index + 1] == b')' {
                    captures.push(true);
                    index += 2;
                } else {
                    captures.push(false);
                    open_captures += 1;
                    index += 1;
                }
            }
            b')' => {
                if open_captures == 0 {
                    return None;
                }
                open_captures -= 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    (open_captures == 0).then_some(captures)
}

pub fn is_plain_index_function_name(expr: &Expr) -> bool {
    match expr {
        Expr::IndexName { op, .. } => *op == IndexOp::Dot,
        Expr::IndexExpr { .. } => true,
        Expr::Group { expr, .. } => is_plain_index_function_name(expr),
        _ => false,
    }
}

pub fn explicit_table_builtin_method(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::IndexName { expr, index, .. }
            if matches!(expr.as_ref(), Expr::Global { name, .. } if name.as_str() == "table")
                && matches!(index.as_str(), "create" | "find" | "unpack") =>
        {
            Some(index.as_str())
        }
        Expr::Group { expr, .. } => explicit_table_builtin_method(expr),
        _ => None,
    }
}

pub fn expr_contains_any_syntax(
    expr: &Expr,
    syntax_ids: &crate::fastmap::FastSet<SyntaxId>,
) -> bool {
    if syntax_ids.contains(&expr.syntax_id()) {
        return true;
    }
    match expr {
        Expr::Call { func, args, .. } => {
            expr_contains_any_syntax(func, syntax_ids)
                || args
                    .iter()
                    .any(|arg| expr_contains_any_syntax(arg, syntax_ids))
        }
        Expr::Binary { left, right, .. } => {
            expr_contains_any_syntax(left, syntax_ids)
                || expr_contains_any_syntax(right, syntax_ids)
        }
        Expr::Unary { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::IndexName { expr, .. }
        | Expr::Group { expr, .. }
        | Expr::Instantiate { expr, .. } => expr_contains_any_syntax(expr, syntax_ids),
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            expr_contains_any_syntax(condition, syntax_ids)
                || expr_contains_any_syntax(true_expr, syntax_ids)
                || expr_contains_any_syntax(false_expr, syntax_ids)
        }
        Expr::IndexExpr { expr, index, .. } => {
            expr_contains_any_syntax(expr, syntax_ids)
                || expr_contains_any_syntax(index, syntax_ids)
        }
        Expr::Table { items, .. } => items.iter().any(|item| {
            item.key
                .as_ref()
                .is_some_and(|key| expr_contains_any_syntax(key, syntax_ids))
                || expr_contains_any_syntax(&item.value, syntax_ids)
        }),
        Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => expressions
            .iter()
            .any(|expr| expr_contains_any_syntax(expr, syntax_ids)),
        Expr::Function { .. }
        | Expr::Nil { .. }
        | Expr::Bool { .. }
        | Expr::Number { .. }
        | Expr::Integer { .. }
        | Expr::String { .. }
        | Expr::Global { .. }
        | Expr::Local { .. }
        | Expr::Varargs { .. } => false,
    }
}

pub fn expr_is_logical_binary_containing_any_syntax(
    expr: &Expr,
    syntax_ids: &crate::fastmap::FastSet<SyntaxId>,
) -> bool {
    matches!(
        ungroup_expr(expr),
        Expr::Binary {
            op: BinaryOp::And | BinaryOp::Or,
            ..
        } if expr_contains_any_syntax(expr, syntax_ids)
    )
}

pub fn setmetatable_call_metamethod_function_needing_annotation(metatable: &Expr) -> Option<&Expr> {
    let Expr::Table { items, .. } = ungroup_expr(metatable) else {
        return None;
    };
    items
        .iter()
        .find(|item| static_table_item_key(item).as_deref() == Some("__call"))
        .map(|item| &item.value)
        .and_then(|value| {
            let value = ungroup_expr(value);
            call_metamethod_function_needs_annotation(value).then_some(value)
        })
}

fn call_metamethod_function_needs_annotation(function: &Expr) -> bool {
    let Expr::Function {
        generics,
        generic_packs,
        args,
        self_arg,
        vararg,
        return_annotation,
        body,
        ..
    } = function
    else {
        return false;
    };
    if !generics.is_empty()
        || !generic_packs.is_empty()
        || self_arg.is_some()
        || *vararg
        || return_annotation.is_some()
    {
        return false;
    }
    let Some(receiver) = args.first().filter(|arg| arg.annotation.is_none()) else {
        return false;
    };
    stat_returns_arithmetic_property_of_local(body, receiver.id)
}

fn stat_returns_arithmetic_property_of_local(stat: &Stat, local_id: LocalId) -> bool {
    match stat {
        Stat::Block { body, .. } => body
            .iter()
            .any(|stat| stat_returns_arithmetic_property_of_local(stat, local_id)),
        Stat::Return { list, .. } => list
            .iter()
            .any(|expr| expr_arithmetic_uses_property_of_local(expr, local_id)),
        Stat::If {
            then_body,
            else_body,
            ..
        } => {
            stat_returns_arithmetic_property_of_local(then_body, local_id)
                || else_body
                    .as_deref()
                    .is_some_and(|stat| stat_returns_arithmetic_property_of_local(stat, local_id))
        }
        Stat::While { body, .. }
        | Stat::Repeat { body, .. }
        | Stat::For { body, .. }
        | Stat::ForIn { body, .. } => stat_returns_arithmetic_property_of_local(body, local_id),
        _ => false,
    }
}

fn expr_arithmetic_uses_property_of_local(expr: &Expr, local_id: LocalId) -> bool {
    match ungroup_expr(expr) {
        Expr::Binary {
            op:
                BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::FloorDiv
                | BinaryOp::Mod
                | BinaryOp::Pow,
            left,
            right,
            ..
        } => {
            expr_is_unasserted_property_of_local(left, local_id)
                || expr_is_unasserted_property_of_local(right, local_id)
                || expr_arithmetic_uses_property_of_local(left, local_id)
                || expr_arithmetic_uses_property_of_local(right, local_id)
        }
        Expr::Binary { left, right, .. } => {
            expr_arithmetic_uses_property_of_local(left, local_id)
                || expr_arithmetic_uses_property_of_local(right, local_id)
        }
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            expr_arithmetic_uses_property_of_local(condition, local_id)
                || expr_arithmetic_uses_property_of_local(true_expr, local_id)
                || expr_arithmetic_uses_property_of_local(false_expr, local_id)
        }
        Expr::Call { func, args, .. } => {
            expr_arithmetic_uses_property_of_local(func, local_id)
                || args
                    .iter()
                    .any(|arg| expr_arithmetic_uses_property_of_local(arg, local_id))
        }
        Expr::Unary { expr, .. }
        | Expr::IndexName { expr, .. }
        | Expr::Group { expr, .. }
        | Expr::Instantiate { expr, .. } => expr_arithmetic_uses_property_of_local(expr, local_id),
        Expr::IndexExpr { expr, index, .. } => {
            expr_arithmetic_uses_property_of_local(expr, local_id)
                || expr_arithmetic_uses_property_of_local(index, local_id)
        }
        Expr::Table { items, .. } => items.iter().any(|item| {
            item.key
                .as_ref()
                .is_some_and(|key| expr_arithmetic_uses_property_of_local(key, local_id))
                || expr_arithmetic_uses_property_of_local(&item.value, local_id)
        }),
        Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => expressions
            .iter()
            .any(|expr| expr_arithmetic_uses_property_of_local(expr, local_id)),
        Expr::TypeAssertion { .. }
        | Expr::Function { .. }
        | Expr::Nil { .. }
        | Expr::Bool { .. }
        | Expr::Number { .. }
        | Expr::Integer { .. }
        | Expr::String { .. }
        | Expr::Global { .. }
        | Expr::Local { .. }
        | Expr::Varargs { .. } => false,
    }
}

fn expr_is_unasserted_property_of_local(expr: &Expr, local_id: LocalId) -> bool {
    match ungroup_expr(expr) {
        Expr::IndexName { expr, .. } | Expr::IndexExpr { expr, .. } => {
            expr_is_local_reference(expr, local_id)
                || expr_is_unasserted_property_of_local(expr, local_id)
        }
        Expr::Group { expr, .. } => expr_is_unasserted_property_of_local(expr, local_id),
        Expr::TypeAssertion { .. } => false,
        _ => false,
    }
}

fn expr_is_local_reference(expr: &Expr, local_id: LocalId) -> bool {
    matches!(ungroup_expr(expr), Expr::Local { local, .. } if local.id == local_id)
}

pub fn is_table_insert_call(expr: &Expr) -> bool {
    match ungroup_expr(expr) {
        Expr::IndexName {
            expr, index, op, ..
        } if *op == IndexOp::Dot && index.as_str() == "insert" => {
            matches!(ungroup_expr(expr), Expr::Global { name, .. } if name.as_str() == "table")
        }
        _ => false,
    }
}

pub fn is_table_clone_call(expr: &Expr) -> bool {
    match ungroup_expr(expr) {
        Expr::IndexName {
            expr, index, op, ..
        } if *op == IndexOp::Dot && index.as_str() == "clone" => {
            matches!(ungroup_expr(expr), Expr::Global { name, .. } if name.as_str() == "table")
        }
        _ => false,
    }
}

pub(super) fn expr_is_table_freeze_call(expr: &Expr) -> bool {
    match ungroup_expr(expr) {
        Expr::Call { func, is_self, .. } => !*is_self && is_table_freeze_call(func),
        _ => false,
    }
}

pub fn is_table_freeze_call(expr: &Expr) -> bool {
    match ungroup_expr(expr) {
        Expr::IndexName {
            expr, index, op, ..
        } if *op == IndexOp::Dot && index.as_str() == "freeze" => {
            matches!(ungroup_expr(expr), Expr::Global { name, .. } if name.as_str() == "table")
        }
        _ => false,
    }
}

pub fn is_string_format_function_value(expr: &Expr) -> bool {
    match ungroup_expr(expr) {
        Expr::IndexName {
            expr, index, op, ..
        } if *op == IndexOp::Dot && index.as_str() == "format" => is_string_global(expr),
        _ => false,
    }
}

pub fn is_string_global(expr: &Expr) -> bool {
    matches!(ungroup_expr(expr), Expr::Global { name, .. } if name.as_str() == "string")
}

#[derive(Clone, Copy)]
pub enum CallArgumentSupply {
    Finite(usize),
    VariadicTail,
}

impl CallArgumentSupply {
    pub(crate) fn from_parts(fixed_count: usize, tail: Option<TypePackId>) -> Self {
        if tail.is_some() {
            Self::VariadicTail
        } else {
            Self::Finite(fixed_count)
        }
    }

    pub(crate) fn definite_count(self) -> Option<usize> {
        match self {
            Self::Finite(count) => Some(count),
            Self::VariadicTail => None,
        }
    }

    pub(crate) fn count_for_missing_bindings(self) -> usize {
        self.definite_count().unwrap_or(usize::MAX)
    }
}

fn local_initializer_defers_expected_check(expr: &Expr) -> bool {
    matches!(
        ungroup_expr(expr),
        Expr::Local { .. }
            | Expr::String { .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::Bool { .. }
            | Expr::Nil { .. }
    )
}

pub fn callee_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Global { name, .. } => Some(name.as_str()),
        Expr::Local { local, .. } => Some(local.name.as_str()),
        _ => None,
    }
}
