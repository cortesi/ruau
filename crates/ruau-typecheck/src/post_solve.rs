//! Post-solve strict statement checks.

use ruau_syntax::{BinaryOp, Expr, Location, Stat};

use crate::{
    diagnostics::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Diagnostics, Payload},
    generation::operator::{
        binary_metamethod_name, is_relational_operator, relational_operator_text,
    },
    graph::Mode,
    queries::Queries,
    subtype::definitely_uninhabited_type,
    types::{Arena, PrimitiveType, SingletonType, TypeId, TypeKind},
};

/// Runs strict post-solve checks over a checked module.
#[must_use]
pub fn check_strict_statements(root: &Stat, mode: Mode) -> Diagnostics {
    if mode != Mode::Strict {
        return Diagnostics::new();
    }

    let mut checker = PostSolveChecker {
        diagnostics: Diagnostics::new(),
        function_depth: 0,
        check_lvalues: true,
        solved: None,
    };
    checker.visit_stat(root);
    checker.diagnostics
}

/// Runs post-solve expression checks that need final inferred types.
#[must_use]
pub fn check_solved_expressions(
    root: &Stat,
    mode: Mode,
    queries: &Queries,
    arena: &Arena,
) -> Diagnostics {
    if mode == Mode::NoCheck {
        return Diagnostics::new();
    }

    let mut checker = PostSolveChecker {
        diagnostics: Diagnostics::new(),
        function_depth: 0,
        check_lvalues: false,
        solved: Some(SolvedExpressionContext { queries, arena }),
    };
    checker.visit_stat(root);
    checker.diagnostics
}

struct SolvedExpressionContext<'a> {
    queries: &'a Queries,
    arena: &'a Arena,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelationalOperandKind {
    Number,
    String,
    Unknown,
    Invalid,
}

fn relational_operands_are_valid(
    solved: &SolvedExpressionContext<'_>,
    op: BinaryOp,
    left: TypeId,
    right: TypeId,
) -> bool {
    if definitely_uninhabited_type(solved.arena, left)
        || definitely_uninhabited_type(solved.arena, right)
        || has_matching_relational_metamethod(solved, op, left, right)
    {
        return true;
    }
    let left_kind = relational_operand_kind(solved.arena, left);
    let right_kind = relational_operand_kind(solved.arena, right);
    matches!(left_kind, RelationalOperandKind::Unknown)
        || matches!(right_kind, RelationalOperandKind::Unknown)
        || left_kind == right_kind
            && matches!(
                left_kind,
                RelationalOperandKind::Number | RelationalOperandKind::String
            )
        || relational_operand_orderable_union(solved.arena, left)
            == relational_operand_orderable_union(solved.arena, right)
            && relational_operand_orderable_union(solved.arena, left).is_some()
}

fn has_matching_relational_metamethod(
    solved: &SolvedExpressionContext<'_>,
    op: BinaryOp,
    left: TypeId,
    right: TypeId,
) -> bool {
    let Some(metamethod) = binary_metamethod_name(op) else {
        return false;
    };
    let Some(left_metatable) = solved.arena.metatable_payload(left) else {
        return false;
    };
    let Some(right_metatable) = solved.arena.metatable_payload(right) else {
        return false;
    };
    solved.arena.follow(left_metatable) == solved.arena.follow(right_metatable)
        && metatable_property_type(solved.arena, left_metatable, metamethod)
}

fn relational_metatables_differ(arena: &Arena, left: TypeId, right: TypeId) -> bool {
    let left_metatable = arena.metatable_payload(left);
    let right_metatable = arena.metatable_payload(right);
    (left_metatable.is_some() || right_metatable.is_some()) && left_metatable != right_metatable
}

fn metatable_property_type(arena: &Arena, metatable: TypeId, property: &str) -> bool {
    match arena.get(arena.follow(metatable)) {
        TypeKind::Table(table) => table.properties.contains_key(property),
        TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => true,
        _ => false,
    }
}

fn relational_operand_kind(arena: &Arena, ty: TypeId) -> RelationalOperandKind {
    match arena.get(arena.follow(ty)) {
        TypeKind::Primitive(PrimitiveType::Number) => RelationalOperandKind::Number,
        TypeKind::Primitive(PrimitiveType::String)
        | TypeKind::Singleton(SingletonType::String(_)) => RelationalOperandKind::String,
        TypeKind::Any
        | TypeKind::Unknown
        | TypeKind::Error
        | TypeKind::Blocked(_)
        | TypeKind::Free(_) => RelationalOperandKind::Unknown,
        TypeKind::Union(types) => {
            let mut kind = None;
            for ty in types {
                let option = relational_operand_kind(arena, *ty);
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
        _ => RelationalOperandKind::Invalid,
    }
}

fn relational_operand_orderable_union(
    arena: &Arena,
    ty: TypeId,
) -> Option<std::collections::BTreeSet<PrimitiveType>> {
    let TypeKind::Union(types) = arena.get(arena.follow(ty)) else {
        return None;
    };
    let mut primitives = std::collections::BTreeSet::new();
    for ty in types {
        match relational_operand_kind(arena, *ty) {
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

fn type_was_solver_resolved(arena: &Arena, ty: TypeId) -> bool {
    matches!(
        arena.get(ty),
        TypeKind::Bound(_) | TypeKind::Free(_) | TypeKind::Blocked(_)
    )
}

struct PostSolveChecker<'a> {
    diagnostics: Diagnostics,
    function_depth: usize,
    check_lvalues: bool,
    solved: Option<SolvedExpressionContext<'a>>,
}

impl PostSolveChecker<'_> {
    fn visit_stat(&mut self, stat: &Stat) {
        match stat {
            Stat::Block { body, .. } => {
                for stat in body {
                    self.visit_stat(stat);
                }
            }
            Stat::Return { list, .. } => {
                for expr in list {
                    self.visit_expr(expr);
                }
            }
            Stat::Expr { expr, .. } => self.visit_expr(expr),
            Stat::Local { vars, values, .. } => {
                for local in vars {
                    if let Some(annotation) = &local.annotation {
                        self.visit_type(annotation);
                    }
                }
                for value in values {
                    self.visit_expr(value);
                }
            }
            Stat::Assign { vars, values, .. } => {
                for var in vars {
                    if self.check_lvalues {
                        self.check_lvalue(var);
                    }
                    self.visit_expr(var);
                }
                for value in values {
                    self.visit_expr(value);
                }
            }
            Stat::CompoundAssign { var, value, .. } => {
                if self.check_lvalues {
                    self.check_lvalue(var);
                }
                self.visit_expr(var);
                self.visit_expr(value);
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.visit_expr(condition);
                self.visit_stat(then_body);
                if let Some(else_body) = else_body {
                    self.visit_stat(else_body);
                }
            }
            Stat::Break { .. } | Stat::Continue { .. } => {}
            Stat::While {
                condition, body, ..
            } => {
                self.visit_expr(condition);
                self.visit_stat(body);
            }
            Stat::Repeat {
                condition, body, ..
            } => {
                self.visit_stat(body);
                self.visit_expr(condition);
            }
            Stat::For {
                from,
                to,
                step,
                body,
                ..
            } => {
                self.visit_expr(from);
                self.visit_expr(to);
                if let Some(step) = step {
                    self.visit_expr(step);
                }
                self.visit_stat(body);
            }
            Stat::ForIn { values, body, .. } => {
                for value in values {
                    self.visit_expr(value);
                }
                self.visit_stat(body);
            }
            Stat::Function { name, func, .. } => {
                if self.check_lvalues {
                    self.check_lvalue(name);
                }
                self.visit_expr(name);
                if self.check_lvalues {
                    self.visit_function_expr(func);
                } else {
                    self.visit_expr(func);
                }
            }
            Stat::LocalFunction { func, .. } => self.visit_function_expr(func),
            Stat::DeclareGlobal { declared_type, .. } => self.visit_type(declared_type),
            Stat::DeclareFunction {
                params, ret_types, ..
            } => {
                for ty in &params.types {
                    self.visit_type(ty);
                }
                if let Some(tail) = &params.tail_type {
                    self.visit_type_pack(tail);
                }
                self.visit_type_pack(ret_types);
            }
            Stat::DeclareClass { props, .. } => {
                for prop in props {
                    self.visit_type(&prop.declared_type);
                }
            }
            Stat::TypeAlias { value, .. } => self.visit_type(value),
            Stat::ClassProperty {
                declared_type: Some(value),
                ..
            } => self.visit_type(value),
            Stat::ClassProperty {
                declared_type: None,
                ..
            } => {}
            Stat::TypeFunction { func, .. } => self.visit_expr(func),
            Stat::Class { members, .. } => {
                for member in members {
                    self.visit_stat(member);
                }
            }
            Stat::Error {
                expressions,
                statements,
                ..
            } => {
                for expr in expressions {
                    self.visit_expr(expr);
                }
                for stat in statements {
                    self.visit_stat(stat);
                }
            }
        }
    }

    fn visit_function_expr(&mut self, expr: &Expr) {
        if !matches!(expr, Expr::Function { .. }) {
            self.diagnostics.push(diagnostic(
                DiagnosticCategory::Internal,
                expr.location(),
                "function statement did not contain a function expression",
            ));
        }
        self.visit_expr(expr);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Nil { .. }
            | Expr::Bool { .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::String { .. }
            | Expr::Global { .. }
            | Expr::Local { .. }
            | Expr::Varargs { .. } => {}
            Expr::Call { func, args, .. } => {
                self.visit_expr(func);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            Expr::Binary {
                op,
                left,
                right,
                location,
                ..
            } => {
                self.check_relational_binary(*op, *location, left, right);
                self.visit_expr(left);
                self.visit_expr(right);
            }
            Expr::Unary { expr, .. } | Expr::Group { expr, .. } => self.visit_expr(expr),
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                self.visit_expr(condition);
                self.visit_expr(true_expr);
                self.visit_expr(false_expr);
            }
            Expr::TypeAssertion {
                expr, annotation, ..
            } => {
                self.visit_expr(expr);
                self.visit_type(annotation);
            }
            Expr::IndexName { expr, .. } => self.visit_expr(expr),
            Expr::IndexExpr { expr, index, .. } => {
                self.visit_expr(expr);
                self.visit_expr(index);
            }
            Expr::Table { items, .. } => {
                for item in items {
                    if let Some(key) = &item.key {
                        self.visit_expr(key);
                    }
                    self.visit_expr(&item.value);
                }
            }
            Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => {
                for expr in expressions {
                    self.visit_expr(expr);
                }
            }
            Expr::Function {
                args,
                self_arg,
                vararg_annotation,
                return_annotation,
                body,
                ..
            } => {
                self.function_depth += 1;
                if let Some(self_arg) = self_arg
                    && let Some(annotation) = &self_arg.annotation
                {
                    self.visit_type(annotation);
                }
                for arg in args {
                    if let Some(annotation) = &arg.annotation {
                        self.visit_type(annotation);
                    }
                }
                if let Some(vararg_annotation) = vararg_annotation {
                    self.visit_type_pack(vararg_annotation);
                }
                if let Some(return_annotation) = return_annotation {
                    self.visit_type_pack(return_annotation);
                }
                self.visit_stat(body);
                self.function_depth -= 1;
            }
            Expr::Instantiate { expr, .. } => self.visit_expr(expr),
        }
    }

    fn check_lvalue(&mut self, expr: &Expr) {
        match expr {
            Expr::Global { .. } | Expr::IndexName { .. } | Expr::IndexExpr { .. } => {}
            Expr::Local {
                location, local, ..
            } if local.is_const => self.diagnostics.push(diagnostic(
                DiagnosticCategory::TypeMismatch,
                *location,
                format!("cannot assign to constant local `{}`", local.name.as_str()),
            )),
            Expr::Local { .. } => {}
            _ => self.diagnostics.push(diagnostic(
                DiagnosticCategory::TypeMismatch,
                expr.location(),
                "expression is not assignable",
            )),
        }
    }

    fn check_relational_binary(
        &mut self,
        op: BinaryOp,
        location: Option<Location>,
        left: &Expr,
        right: &Expr,
    ) {
        if !is_relational_operator(op) {
            return;
        }

        let (left_summary, right_summary, metatables_differ) = {
            let Some(solved) = self.solved.as_ref() else {
                return;
            };
            let Some(left_ty) = solved.queries.actual_by_syntax(left.syntax_id()) else {
                return;
            };
            let Some(right_ty) = solved.queries.actual_by_syntax(right.syntax_id()) else {
                return;
            };
            if !type_was_solver_resolved(solved.arena, left_ty)
                && !type_was_solver_resolved(solved.arena, right_ty)
            {
                return;
            }
            if relational_operands_are_valid(solved, op, left_ty, right_ty) {
                return;
            }
            (
                solved.arena.summary(left_ty),
                solved.arena.summary(right_ty),
                relational_metatables_differ(solved.arena, left_ty, right_ty),
            )
        };

        let mut diagnostic = Diagnostic::binary_operator_error(
            relational_operator_text(op),
            left_summary.clone(),
            right_summary.clone(),
            "relational",
        );
        // Emit at the binary expression's location — the same location the
        // generation-time relational check uses — so the two channels dedup
        // instead of double-counting.
        diagnostic.primary_location = DiagnosticLocation::from_opt(location);
        if metatables_differ {
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
        self.diagnostics.push(diagnostic);
    }

    fn visit_type(&mut self, ty: &ruau_syntax::Type) {
        match ty {
            ruau_syntax::Type::Reference { parameters, .. } => {
                for parameter in parameters {
                    match parameter {
                        ruau_syntax::TypeParameter::Type(ty) => self.visit_type(ty),
                        ruau_syntax::TypeParameter::Pack(pack) => self.visit_type_pack(pack),
                    }
                }
            }
            ruau_syntax::Type::Typeof { expr, .. } => self.visit_expr(expr),
            ruau_syntax::Type::Optional { .. }
            | ruau_syntax::Type::SingletonString { .. }
            | ruau_syntax::Type::SingletonBool { .. } => {}
            ruau_syntax::Type::Group { inner, .. } => self.visit_type(inner),
            ruau_syntax::Type::Union { types, .. }
            | ruau_syntax::Type::Intersection { types, .. }
            | ruau_syntax::Type::Error { types, .. } => {
                for ty in types {
                    self.visit_type(ty);
                }
            }
            ruau_syntax::Type::Function {
                arg_types,
                return_types,
                ..
            } => {
                for ty in &arg_types.types {
                    self.visit_type(ty);
                }
                if let Some(tail) = &arg_types.tail_type {
                    self.visit_type_pack(tail);
                }
                self.visit_type_pack(return_types);
            }
            ruau_syntax::Type::Table { props, indexer, .. } => {
                for prop in props {
                    self.visit_type(&prop.prop_type);
                }
                if let Some(indexer) = indexer {
                    self.visit_type(&indexer.index_type);
                    self.visit_type(&indexer.result_type);
                }
            }
        }
    }

    fn visit_type_pack(&mut self, pack: &ruau_syntax::TypePack) {
        match pack {
            ruau_syntax::TypePack::Explicit { type_list, .. } => {
                for ty in &type_list.types {
                    self.visit_type(ty);
                }
                if let Some(tail) = &type_list.tail_type {
                    self.visit_type_pack(tail);
                }
            }
            ruau_syntax::TypePack::Variadic { variadic_type, .. } => {
                self.visit_type(variadic_type);
            }
            ruau_syntax::TypePack::Generic { .. } => {}
        }
    }
}

fn diagnostic(
    category: DiagnosticCategory,
    location: Option<Location>,
    context: impl Into<String>,
) -> Diagnostic {
    Diagnostic::error(category, DiagnosticLocation::from_opt(location)).with_context(context)
}

#[cfg(any())]
mod tests {
    use ruau_syntax::SyntaxId;

    use super::*;

    fn invalid_assignment_root() -> Stat {
        Stat::Assign {
            location: None,
            vars: vec![Expr::Nil {
                syntax_id: SyntaxId::new(1),
                location: None,
            }],
            values: Vec::new(),
        }
    }

    #[test]
    fn strict_post_solve_reports_non_assignable_lvalues() {
        let diagnostics = check_strict_statements(&invalid_assignment_root(), Mode::Strict);

        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.category == DiagnosticCategory::TypeMismatch
                    && diagnostic
                        .context
                        .as_deref()
                        .is_some_and(|context| context.contains("not assignable"))
            }),
            "expected invalid-assignment diagnostic, got {diagnostics:?}"
        );
    }

    #[test]
    fn non_strict_modes_skip_strict_post_solve_checks() {
        assert!(check_strict_statements(&invalid_assignment_root(), Mode::NoCheck).is_empty());
        assert!(check_strict_statements(&invalid_assignment_root(), Mode::Nonstrict).is_empty());
    }
}
