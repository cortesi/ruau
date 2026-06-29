//! Expression constraint generation for single-module checking.

use std::collections::{BTreeMap, BTreeSet};

use ruau_analysis::resolve::AnalysisMode;
use ruau_ast::{
    Location,
    json::JsonCompoundAssignOp,
    syntax::{Expr, Local, LocalId, LocalRef, Stat, SyntaxId, Type},
    visit::{NodePath, Visitor, WalkControl, walk_expr, walk_stat, walk_type, walk_type_pack},
};

use crate::{
    ast_util::ungroup_expr,
    checker::GenerationConfig,
    constraints::{Constraint, ConstraintSolveError},
    dfg::{DataFlowGraph, RefinementKey, RefinementMap},
    diagnostics::{Diagnostic, DiagnosticCategory, DiagnosticLocation},
    generalize::{
        function_signature_has_callback_free_correlation, generalize_function_frees,
        generalize_function_signature_frees,
    },
    generation::{
        expression::{
            expr_contains_any_syntax, expr_is_logical_binary_containing_any_syntax,
            expr_is_table_freeze_call, expr_is_table_literal, is_plain_index_function_name,
            is_string_format_function_value, widened_table_literal_value_type,
        },
        state::{
            ExpressionConstraintGenerator, GeneratedConstraints, InferredReturnPath,
            InferredReturnType,
        },
        type_function_eval::type_function_needs_eager_singleton_validation,
    },
    scopes::{ScopeId, ScopeTree, Symbol, TypeBindingKind, ValueBindingKind},
    subtype::{SubtypeError, SubtypeErrorKind, SubtypeTarget, Subtyper},
    types::{
        Arena, FunctionType, TableProperty, TableState, TableType, TypeId, TypeKind, TypeLevel,
        TypePackId, TypePackKind, TypePackTail, TypePath, TypePathComponent, is_top_function_type,
    },
    unify::Unifier,
};

struct FunctionBodyPropertyReadVisitor<'a> {
    base: &'a Expr,
    property: &'a str,
    found: bool,
}

struct FunctionBodySetmetatableLocalPropertyReadVisitor<'a> {
    base: &'a Expr,
    property: &'a str,
    metatable_locals: BTreeSet<LocalId>,
    found: bool,
}

struct FunctionBodyBaseReadVisitor<'a> {
    base: &'a Expr,
    found: bool,
}

struct FunctionBodyReturnVisitor {
    found: bool,
}

struct SelfMethodCallVisitor {
    self_id: LocalId,
    properties: BTreeSet<String>,
}

struct GlobalReadVisitor<'a> {
    global: &'a str,
    found: bool,
}

/// Globals bound in a `type function` body's runtime environment. Mirrors
/// upstream's `typeFunctionRuntimeBindings` (BuiltinDefinitions.cpp) plus the
/// `types` library; any other global read is unknown at definition time.
const TYPE_FUNCTION_RUNTIME_GLOBALS: &[&str] = &[
    "types",
    "math",
    "table",
    "string",
    "bit32",
    "utf8",
    "buffer",
    "assert",
    "error",
    "print",
    "next",
    "ipairs",
    "pairs",
    "select",
    "unpack",
    "getmetatable",
    "setmetatable",
    "rawget",
    "rawset",
    "rawlen",
    "rawequal",
    "tonumber",
    "tostring",
    "type",
    "typeof",
];

#[derive(Default)]
struct TypeFunctionGlobalCollector {
    seen: BTreeSet<String>,
    globals: Vec<(String, SyntaxId, Option<Location>)>,
}

impl Visitor<'_> for TypeFunctionGlobalCollector {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        if let Expr::Global {
            name,
            syntax_id,
            location,
        } = expr
            && self.seen.insert(name.as_str().to_owned())
        {
            self.globals
                .push((name.as_str().to_owned(), *syntax_id, *location));
        }
        WalkControl::Continue
    }
}

fn query_initializer_widens_singleton(expr: &Expr) -> bool {
    matches!(
        ungroup_expr(expr),
        Expr::String { .. }
            | Expr::Bool { .. }
            | Expr::IfElse { .. }
            | Expr::IndexName { .. }
            | Expr::IndexExpr { .. }
    )
}

impl Visitor<'_> for FunctionBodyPropertyReadVisitor<'_> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        if self.found {
            return WalkControl::SkipChildren;
        }
        match expr {
            Expr::IndexName {
                expr: base, index, ..
            } if index.as_str() == self.property && same_named_table_base(base, self.base) => {
                self.found = true;
                WalkControl::SkipChildren
            }
            Expr::Function { .. } => WalkControl::SkipChildren,
            _ => WalkControl::Continue,
        }
    }
}

impl Visitor<'_> for FunctionBodySetmetatableLocalPropertyReadVisitor<'_> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        if self.found {
            return WalkControl::SkipChildren;
        }
        match expr {
            Expr::Call { func, args, .. } => {
                if matches!(
                    ungroup_expr(func),
                    Expr::Global { name, .. } if name.as_str() == "setmetatable"
                ) && let [first, second, ..] = args.as_slice()
                    && let Expr::Local { local, .. } = ungroup_expr(first)
                    && same_named_table_base(second, self.base)
                {
                    self.metatable_locals.insert(local.id);
                }
                WalkControl::Continue
            }
            Expr::IndexName {
                expr: base, index, ..
            } if index.as_str() == self.property
                && matches!(
                    ungroup_expr(base),
                    Expr::Local { local, .. } if self.metatable_locals.contains(&local.id)
                ) =>
            {
                self.found = true;
                WalkControl::SkipChildren
            }
            _ => WalkControl::Continue,
        }
    }
}

impl Visitor<'_> for GlobalReadVisitor<'_> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        if self.found {
            return WalkControl::SkipChildren;
        }
        if matches!(expr, Expr::Global { name, .. } if name.as_str() == self.global) {
            self.found = true;
            return WalkControl::SkipChildren;
        }
        WalkControl::Continue
    }
}

impl Visitor<'_> for FunctionBodyBaseReadVisitor<'_> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        if self.found {
            return WalkControl::SkipChildren;
        }
        if same_named_table_base(expr, self.base) {
            self.found = true;
            WalkControl::SkipChildren
        } else if matches!(expr, Expr::Function { .. }) {
            WalkControl::SkipChildren
        } else {
            WalkControl::Continue
        }
    }
}

impl Visitor<'_> for FunctionBodyReturnVisitor {
    fn visit_stat(&mut self, _path: &NodePath, stat: &Stat) -> WalkControl {
        if self.found {
            return WalkControl::SkipChildren;
        }
        if matches!(stat, Stat::Return { .. }) {
            self.found = true;
            WalkControl::SkipChildren
        } else {
            WalkControl::Continue
        }
    }

    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        if matches!(expr, Expr::Function { .. }) {
            WalkControl::SkipChildren
        } else {
            WalkControl::Continue
        }
    }
}

impl Visitor<'_> for SelfMethodCallVisitor {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        match expr {
            Expr::Call { func, .. } => {
                if let Expr::IndexName {
                    expr: base,
                    index,
                    op,
                    ..
                } = ungroup_expr(func)
                    && *op == ":"
                    && matches!(ungroup_expr(base), Expr::Local { local, .. } if local.id == self.self_id)
                {
                    self.properties.insert(index.as_str().to_owned());
                }
                WalkControl::Continue
            }
            Expr::Function { .. } => WalkControl::SkipChildren,
            _ => WalkControl::Continue,
        }
    }
}

fn function_body_reads_property(func: &Expr, base: &Expr, property: &str) -> bool {
    let Expr::Function { body, .. } = func else {
        return false;
    };
    let mut visitor = FunctionBodyPropertyReadVisitor {
        base,
        property,
        found: false,
    };
    walk_stat(body, &mut visitor);
    visitor.found
}

fn function_body_reads_setmetatable_local_property(
    func: &Expr,
    base: &Expr,
    property: &str,
) -> bool {
    let Expr::Function { body, .. } = func else {
        return false;
    };
    let mut visitor = FunctionBodySetmetatableLocalPropertyReadVisitor {
        base,
        property,
        metatable_locals: BTreeSet::new(),
        found: false,
    };
    walk_stat(body, &mut visitor);
    visitor.found
}

fn function_body_reads_base(func: &Expr, base: &Expr) -> bool {
    let Expr::Function { body, .. } = func else {
        return false;
    };
    let mut visitor = FunctionBodyBaseReadVisitor { base, found: false };
    walk_stat(body, &mut visitor);
    visitor.found
}

fn function_body_has_return(func: &Expr) -> bool {
    let Expr::Function { body, .. } = func else {
        return false;
    };
    let mut visitor = FunctionBodyReturnVisitor { found: false };
    walk_stat(body, &mut visitor);
    visitor.found
}

fn function_signature_reads_base(func: &Expr, base: &Expr) -> bool {
    let Expr::Function {
        generics,
        generic_packs,
        args,
        self_arg,
        vararg_annotation,
        return_annotation,
        ..
    } = func
    else {
        return false;
    };

    let mut visitor = FunctionBodyBaseReadVisitor { base, found: false };
    for generic in generics {
        if let Some(luau_type) = &generic.luau_type {
            walk_type(luau_type, &mut visitor);
        }
    }
    for generic in generic_packs {
        if let Some(luau_type) = &generic.luau_type {
            walk_type_pack(luau_type, &mut visitor);
        }
    }
    for arg in args {
        if let Some(luau_type) = &arg.luau_type {
            walk_type(luau_type, &mut visitor);
        }
    }
    if let Some(self_arg) = self_arg
        && let Some(luau_type) = &self_arg.luau_type
    {
        walk_type(luau_type, &mut visitor);
    }
    if let Some(vararg_annotation) = vararg_annotation {
        walk_type_pack(vararg_annotation, &mut visitor);
    }
    if let Some(return_annotation) = return_annotation {
        walk_type_pack(return_annotation, &mut visitor);
    }
    visitor.found
}

fn function_references_base(func: &Expr, base: &Expr) -> bool {
    function_body_reads_base(func, base) || function_signature_reads_base(func, base)
}

fn function_has_explicit_return_annotation(func: &Expr) -> bool {
    matches!(
        func,
        Expr::Function {
            return_annotation: Some(_),
            ..
        }
    )
}

fn self_method_call_properties(func: &Expr) -> BTreeSet<String> {
    let Expr::Function {
        self_arg: Some(self_arg),
        body,
        ..
    } = func
    else {
        return BTreeSet::new();
    };
    let mut visitor = SelfMethodCallVisitor {
        self_id: self_arg.id,
        properties: BTreeSet::new(),
    };
    walk_stat(body, &mut visitor);
    visitor.properties
}

fn plain_index_function_base(name: &Expr) -> Option<&Expr> {
    match name {
        Expr::IndexName { expr, op, .. } if *op == "." => Some(expr),
        Expr::IndexExpr { expr, .. } => Some(expr),
        Expr::Group { expr, .. } => plain_index_function_base(expr),
        _ => None,
    }
}

fn self_index_function_name(name: &Expr) -> bool {
    match name {
        Expr::IndexName { op, .. } if *op == ":" => true,
        Expr::Group { expr, .. } => self_index_function_name(expr),
        _ => false,
    }
}

fn self_index_function_base(name: &Expr) -> Option<&Expr> {
    match name {
        Expr::IndexName { expr, op, .. } if *op == ":" => Some(expr),
        Expr::Group { expr, .. } => self_index_function_base(expr),
        _ => None,
    }
}

fn same_named_table_base(left: &Expr, right: &Expr) -> bool {
    match (ungroup_expr(left), ungroup_expr(right)) {
        (Expr::Local { local: left, .. }, Expr::Local { local: right, .. }) => left.id == right.id,
        (Expr::Global { name: left, .. }, Expr::Global { name: right, .. }) => {
            left.as_str() == right.as_str()
        }
        (
            Expr::IndexName {
                expr: left_base,
                index: left_index,
                op: left_op,
                ..
            },
            Expr::IndexName {
                expr: right_base,
                index: right_index,
                op: right_op,
                ..
            },
        ) => {
            left_op == right_op
                && left_index.as_str() == right_index.as_str()
                && same_named_table_base(left_base, right_base)
        }
        _ => false,
    }
}

#[allow(clippy::multiple_inherent_impl)]
impl<'a> ExpressionConstraintGenerator<'a> {
    pub(crate) fn visit_stat(&mut self, scope: ScopeId, stat: &Stat) {
        match stat {
            Stat::Block { body, is_do, .. } => {
                let scope = if *is_do {
                    self.enter_child(scope)
                } else {
                    scope
                };
                self.predeclare_block_function_prototypes(scope, body);
                for (index, stat) in body.iter().enumerate() {
                    self.predeclare_table_function_property_prototypes(scope, &body[index..]);
                    self.visit_stat(scope, stat);
                }
            }
            Stat::Return { location, list } => self.stat_return(scope, *location, list),
            Stat::Expr { expr, .. } => {
                let discard_call = matches!(expr.as_ref(), Expr::Call { .. });
                if discard_call {
                    self.calls.discard_call_results.insert(expr.syntax_id());
                    self.calls.statement_call_results.insert(expr.syntax_id());
                }
                self.expr_type_in_refinement_context(scope, expr);
                if discard_call {
                    self.calls.discard_call_results.remove(&expr.syntax_id());
                    self.calls.statement_call_results.remove(&expr.syntax_id());
                }
                if let Some(refinements) = self.assertion_refinements(expr) {
                    self.merge_current_refinements(refinements);
                }
            }
            Stat::Local { vars, values, .. } => self.stat_local(scope, vars, values),
            Stat::Assign { vars, values, .. } => self.stat_assign(scope, vars, values),
            Stat::CompoundAssign {
                location,
                op,
                var,
                value,
                ..
            } => self.stat_compound_assign(scope, *location, *op, var, value),
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => self.stat_if(scope, condition, then_body, else_body.as_deref()),
            Stat::Break { .. } | Stat::Continue { .. } => {}
            Stat::While {
                condition, body, ..
            } => self.stat_while(scope, condition, body),
            Stat::Repeat {
                condition, body, ..
            } => self.stat_repeat(scope, condition, body),
            Stat::For {
                var,
                from,
                to,
                step,
                body,
                ..
            } => self.stat_for(scope, var, from, to, step.as_deref(), body),
            Stat::ForIn {
                vars, values, body, ..
            } => self.stat_for_in(scope, vars, values, body),
            Stat::Function { name, func, .. } => self.stat_function(scope, name, func),
            Stat::LocalFunction { name, func, .. } => {
                let local_ty = self.local_type(name);
                let func_ty =
                    self.with_next_local_function(name.id, |this| this.expr_type(scope, func));
                drop(Unifier::new(self.arena).unify(local_ty, func_ty));
                self.generated
                    .constraints
                    .push(Constraint::unify_default_location(
                        local_ty,
                        func_ty,
                        name.location
                            .or_else(|| func.location())
                            .map(DiagnosticLocation::from),
                    ));
                self.merge_current_refinements(RefinementMap::from([(
                    RefinementKey::Symbol(Symbol::Local(name.id)),
                    func_ty,
                )]));
            }
            Stat::DeclareFunction {
                location,
                attributes,
                name,
                generics,
                generic_packs,
                params,
                param_names,
                ret_types,
                ..
            } => {
                let ty = self.lower_type(
                    scope,
                    &Type::Function {
                        syntax_id: SyntaxId::default(),
                        location: *location,
                        attributes: attributes.clone(),
                        generics: generics.clone(),
                        generic_packs: generic_packs.clone(),
                        arg_types: params.clone(),
                        arg_names: param_names.iter().cloned().map(Some).collect(),
                        return_types: (**ret_types).clone(),
                    },
                );
                self.attach_table_property_documentation(
                    ty,
                    &format!("@test/global/{}", name.as_str()),
                );
                self.generated
                    .global_defs
                    .insert(name.as_str().to_owned(), ty);
            }
            Stat::TypeAlias { .. } => {
                self.enter_child(scope);
            }
            Stat::Class { members, .. } => {
                let class_scope = self.enter_child(scope);
                for member in members {
                    self.visit_stat(class_scope, member);
                }
            }
            Stat::TypeFunction {
                name,
                func,
                location,
                ..
            } => {
                self.enter_child(scope);
                let has_unknown_globals =
                    self.check_type_function_body_unknown_globals(scope, func);
                if !has_unknown_globals && type_function_needs_eager_singleton_validation(func) {
                    self.eager_type_function_definition(scope, name.as_str(), func, *location);
                }
            }
            Stat::DeclareGlobal {
                name, luau_type, ..
            } => {
                let ty = self.lower_type(scope, luau_type);
                self.attach_table_property_documentation(
                    ty,
                    &format!("@test/global/{}", name.as_str()),
                );
                self.generated
                    .global_defs
                    .insert(name.as_str().to_owned(), ty);
            }
            Stat::DeclareClass { .. } | Stat::ClassProperty { .. } => {}
            Stat::Error {
                expressions,
                statements,
                ..
            } => {
                for expr in expressions {
                    self.expr_type(scope, expr);
                }
                for stat in statements {
                    self.visit_stat(scope, stat);
                }
            }
        }
    }

    /// Reports globals read inside a `type function` body that are not bound in
    /// the type-function runtime environment. Upstream type-checks the body at
    /// definition against a restricted global scope, so a bare `number`,
    /// `gcinfo`, an unassigned global, or an out-of-scope sibling type function
    /// is an "Unknown global" error independent of any use site.
    fn check_type_function_body_unknown_globals(&mut self, scope: ScopeId, func: &Expr) -> bool {
        let Expr::Function { body, .. } = func else {
            return false;
        };
        let mut collector = TypeFunctionGlobalCollector::default();
        walk_stat(body, &mut collector);
        let mut has_unknown_globals = false;
        for (name, syntax_id, location) in collector.globals {
            if self.type_function_global_is_bound(scope, &name) {
                continue;
            }
            has_unknown_globals = true;
            let location =
                location.map_or_else(DiagnosticLocation::missing, DiagnosticLocation::from);
            self.report_unknown_symbol(syntax_id, &name, location);
        }
        has_unknown_globals
    }

    fn eager_type_function_definition(
        &mut self,
        scope: ScopeId,
        name: &str,
        func: &Expr,
        location: Option<Location>,
    ) {
        let Expr::Function { args, .. } = func else {
            return;
        };
        if !args.is_empty() {
            return;
        }
        let _ = self.reduce_user_type_function_with_arguments(
            scope,
            name,
            func,
            Vec::new(),
            location.map(DiagnosticLocation::from),
        );
    }

    /// Whether `name` is bound as a value in a `type function` body's runtime
    /// environment: a runtime binding (`types`, `print`, `setmetatable`, ...)
    /// or a user-defined type alias, class, or sibling type function visible
    /// from the definition site. Builtin primitive types (e.g. `number`) are
    /// type bindings but not runtime values, so they are not bound.
    pub(crate) fn type_function_global_is_bound(&self, scope: ScopeId, name: &str) -> bool {
        TYPE_FUNCTION_RUNTIME_GLOBALS.contains(&name)
            || matches!(
                self.input.scopes.lookup_type_with_scope(scope, name),
                Some((_, binding)) if matches!(
                    binding.kind,
                    TypeBindingKind::TypeAlias
                        | TypeBindingKind::ExportedTypeAlias
                        | TypeBindingKind::Class
                        | TypeBindingKind::DeclaredClass
                        | TypeBindingKind::TypeFunction
                )
            )
    }

    fn ascribe_root_local_type_name(
        &mut self,
        scope: ScopeId,
        local: &Local,
        value: Option<&Expr>,
        ty: TypeId,
    ) {
        if scope != self.input.scopes.root() {
            return;
        }
        if value.is_some_and(|value| {
            self.input
                .require_return_types
                .contains_key(&ungroup_expr(value).syntax_id())
        }) {
            return;
        }
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Table(mut table) if table.name.is_none() => {
                table.name = Some(local.name.as_str().to_owned());
                self.arena.replace(ty, TypeKind::Table(table));
            }
            TypeKind::Metatable {
                table,
                metatable,
                name: None,
            } => {
                self.arena.replace(
                    ty,
                    TypeKind::Metatable {
                        table,
                        metatable,
                        name: Some(local.name.as_str().to_owned()),
                    },
                );
            }
            _ => {}
        }
    }

    fn stat_local(&mut self, scope: ScopeId, vars: &[Local], values: &[Expr]) {
        let annotation_types = vars
            .iter()
            .map(|local| {
                if local.luau_type.is_some() {
                    self.local_surface.annotated_locals.insert(local.id);
                } else {
                    self.local_surface
                        .setmetatable_side_effect_locals
                        .insert(local.id);
                }
                local
                    .luau_type
                    .as_ref()
                    .map(|annotation| self.lower_type(scope, annotation))
            })
            .collect::<Vec<_>>();
        if let ([local], [value]) = (vars, values)
            && annotation_types[0].is_none()
            && matches!(value, Expr::Table { .. })
        {
            let local_ty = self.local_type(local);
            let mut table = crate::types::TableType::new(crate::types::TableState::Unsealed);
            if scope == self.input.scopes.root() {
                table.name = Some(local.name.as_str().to_owned());
            }
            let table_ty = self.arena.alloc(TypeKind::Table(table));
            self.prebound_table_literals
                .insert(value.syntax_id(), table_ty);
            self.bind_free_to(local_ty, table_ty);
        }
        for (local, value) in vars.iter().zip(values) {
            if is_string_format_function_value(value) {
                self.local_surface.string_format_aliases.insert(local.id);
            }
        }
        let value_types = self.local_assignment_value_types(scope, values, &annotation_types);
        for (index, local) in vars.iter().enumerate() {
            let local_ty = self.local_type(local);
            if let Some(annotation_ty) = annotation_types[index] {
                self.bind_annotated_local(
                    index,
                    local,
                    values,
                    local_ty,
                    annotation_ty,
                    &value_types,
                );
            } else if let Some((value_ty, is_explicit_nil, _)) =
                value_types.get(index).and_then(|value| *value)
            {
                if is_explicit_nil {
                    self.nil_tracking.initialized_locals.insert(local.id);
                } else if values.len() == 1
                    && matches!(
                        self.arena.get(self.arena.follow(value_ty)),
                        TypeKind::Free(_)
                    )
                {
                    // A local initialized from a single expression — including one
                    // multi-value call expanded across several locals — binds to
                    // the (still-free) result element so the solve propagates the
                    // resolved type to the local. A multi-var `bind_free_to` alone
                    // no-ops while the call's result element is free.
                    self.arena.replace(local_ty, TypeKind::Bound(value_ty));
                    self.generated
                        .constraints
                        .push(Constraint::unify(value_ty, local_ty));
                } else {
                    let value_expr = if values.len() == 1 {
                        values.first()
                    } else {
                        values.get(index)
                    };
                    self.ascribe_root_local_type_name(scope, local, value_expr, value_ty);
                    self.bind_free_to(local_ty, value_ty);
                }
                // A mutable unannotated local's *declared* type widens
                // string/boolean singleton values to their primitives
                // (upstream's rule). Keep this query-only and source-shaped:
                // copying a singleton local keeps the singleton, while literals,
                // conditional expressions, and table/index reads widen.
                // Recorded query-only: `requireType("s")` reports `string`
                // while the binding and value-flow keep the precise singleton,
                // so assignment checks are unchanged.
                if !is_explicit_nil {
                    let value_expr = if values.len() == 1 {
                        values.first()
                    } else {
                        values.get(index)
                    };
                    if value_expr.is_some_and(query_initializer_widens_singleton) {
                        let widened = self.widen_mutable_query_type(value_ty);
                        if widened != self.arena.follow(value_ty) {
                            self.generated.query_local_types.insert(local.id, widened);
                        }
                    }
                }
            } else {
                if local.is_const {
                    self.bind_free_to(local_ty, self.primitives().nil);
                } else {
                    self.nil_tracking.implicit_locals.insert(local.id);
                }
            }
            if Self::local_initializer_refines_value(values, index)
                && let Some((value_ty, _, _)) = value_types.get(index).and_then(|value| *value)
            {
                let refined_ty = if let Some(annotation_ty) = annotation_types[index]
                    && self.is_dynamic_assignment_source(value_ty)
                {
                    annotation_ty
                } else {
                    self.widen_mutable_literal_type(value_ty)
                };
                self.merge_current_refinements(RefinementMap::from([(
                    RefinementKey::Symbol(Symbol::Local(local.id)),
                    refined_ty,
                )]));
            }
            if self.local_initializer_relaxes_nil_guard(values, index) {
                self.nil_tracking
                    .guard_relaxes_to_nil_locals
                    .insert(local.id);
            }
        }
    }

    /// Binds one annotated `local` declaration: records the eager
    /// annotation/value mismatch diagnostic (skipping `if … else …`
    /// initializers handled per-branch) and pushes the appropriate
    /// annotation subtype constraint for the initializer.
    fn bind_annotated_local(
        &mut self,
        index: usize,
        local: &Local,
        values: &[Expr],
        local_ty: TypeId,
        annotation_ty: TypeId,
        value_types: &[Option<(TypeId, bool, bool)>],
    ) {
        let value_expr = if values.len() == 1 {
            values.first()
        } else {
            values.get(index)
        };
        // An `if … then … else …` initializer already checks each branch
        // against the annotation at the branch's own span (the per-span
        // selection keeps those); binding the annotation and stopping
        // here avoids a duplicate assignment-level diagnostic that would
        // mask the per-branch errors.
        let value_is_if_else =
            value_expr.is_some_and(|value| matches!(ungroup_expr(value), Expr::IfElse { .. }));
        let value_location = value_expr.and_then(Self::eager_annotation_value_location);
        let eager_diagnostic = if value_is_if_else {
            None
        } else if let Some((value_ty, _, _)) = value_types.get(index).and_then(|value| *value)
            && !self.is_dynamic_assignment_source(value_ty)
            && !self.is_error_type(annotation_ty)
        {
            self.eager_local_annotation_mismatch(
                value_ty,
                annotation_ty,
                value_location.or(local.location),
                value_location.is_some(),
            )
        } else {
            None
        };
        if value_is_if_else {
            self.bind_expected_type_without_constraints(local.location, local_ty, annotation_ty);
        } else if let Some(diagnostic) = eager_diagnostic {
            self.bind_expected_type_without_constraints(local.location, local_ty, annotation_ty);
            self.generated.deferred_diagnostics.push(diagnostic);
        } else {
            self.expect_type(local.location, local_ty, annotation_ty);
            let deferred_parameter_expected = value_expr.is_some_and(|value_expr| {
                value_types.get(index).and_then(|value| *value).is_some_and(
                    |(_, _, expected_deferred)| {
                        expected_deferred
                            && self.bind_function_parameter_expected_type(value_expr, annotation_ty)
                    },
                )
            });
            if !deferred_parameter_expected
                && let Some((value_ty, _, _)) = value_types.get(index).and_then(|value| *value)
                && !self.is_dynamic_assignment_source(value_ty)
                && !self.is_error_type(annotation_ty)
            {
                if self.is_generic_pack_overload_surface(value_ty)
                    || self.is_intersection_returning_overload_surface(value_ty, annotation_ty)
                    || self.is_extern_indexer_annotation_surface(value_ty, annotation_ty)
                {
                    self.generated
                        .constraints
                        .push(Constraint::expected_subtype(
                            value_ty,
                            annotation_ty,
                            local.location.map(DiagnosticLocation::from),
                            true,
                        ));
                } else {
                    self.push_annotation_subtype_constraint(
                        value_ty,
                        annotation_ty,
                        value_location.or(local.location),
                    );
                }
            }
        }
    }

    fn stat_assign(&mut self, scope: ScopeId, vars: &[Expr], values: &[Expr]) {
        let locals: Vec<_> = vars
            .iter()
            .filter_map(|var| match var {
                Expr::Local { local, .. } => Some(local),
                _ => None,
            })
            .collect();
        if vars.len() > 1 && locals.len() == vars.len() {
            let value_types = self.assignment_value_types(scope, values, vars.len());
            for (index, (local, value)) in locals.into_iter().zip(value_types).enumerate() {
                let var_ty = self
                    .input
                    .dfg
                    .local(local.id)
                    .map(|def| self.input.dfg.get(def).ty)
                    .unwrap_or_else(|| self.recovery_type_at(local.location, "missing local def"));
                let value_location = values
                    .get(index)
                    .and_then(Self::eager_annotation_value_location);
                self.assign_local_type(scope, local, var_ty, value.ty, value_location);
            }
        } else if vars.len() == values.len() {
            for (var, value) in vars.iter().zip(values) {
                if self.input.mode != AnalysisMode::Strict
                    && matches!(value, Expr::Nil { .. })
                    && !self.lvalue_is_simple_binding(var)
                {
                    continue;
                }
                self.assign_lvalue(scope, var, value);
            }
        } else {
            let value_types = self.assignment_value_types(scope, values, vars.len());
            for (var, value) in vars.iter().zip(value_types) {
                if self.input.mode != AnalysisMode::Strict
                    && self.arena.is_nil(value.ty)
                    && !self.lvalue_is_simple_binding(var)
                {
                    continue;
                }
                self.assign_known_type_to_lvalue(scope, var, value.ty);
            }
        }
    }

    fn stat_compound_assign(
        &mut self,
        scope: ScopeId,
        location: Option<Location>,
        op: JsonCompoundAssignOp,
        var: &Expr,
        value: &Expr,
    ) {
        let missing_nonstrict_global = if self.input.mode != AnalysisMode::Strict
            && let Expr::Global { name, .. } = var
        {
            self.input
                .scopes
                .lookup_global(scope, name.as_str())
                .and_then(|binding| binding.ty)
                .or_else(|| self.generated.global_defs.get(name.as_str()).copied())
                .is_none()
        } else {
            false
        };
        if missing_nonstrict_global
            && let Expr::Global {
                name,
                location: global_location,
                ..
            } = var
        {
            let diagnostic_location = DiagnosticLocation::from_opt(*global_location);
            self.report_unknown_symbol(var.syntax_id(), name.as_str(), diagnostic_location);
        }
        let var_ty = match var {
            Expr::Global { name, .. } if missing_nonstrict_global => self
                .with_suppressed_unknown_global(name.as_str(), |this| this.expr_type(scope, var)),
            _ => self.expr_type(scope, var),
        };
        let value_ty = self.expr_type(scope, value);
        let (result_ty, used_metamethod) =
            self.compound_assignment_result_type(op, var_ty, value_ty, location);
        if used_metamethod {
            self.generated.constraints.push(Constraint::subtype(
                result_ty,
                var_ty,
                location.map(DiagnosticLocation::from),
            ));
        }
        self.write_compound_lvalue(scope, var, result_ty);
    }

    fn stat_if(
        &mut self,
        scope: ScopeId,
        condition: &Expr,
        then_body: &Stat,
        else_body: Option<&Stat>,
    ) {
        self.expr_type_in_refinement_context(scope, condition);
        let then_refinements = self.truthy_refinements(condition);
        let else_refinements = self.falsy_refinements(condition);
        let base_refined_locals = self.refinements.locals.clone();
        let then_always_exits = self.stat_always_exits(then_body);
        let else_always_exits =
            else_body.is_some_and(|else_body| self.stat_always_exits(else_body));
        let then_scope = self.enter_child(scope);
        self.refinements.locals.push(then_refinements.clone());
        self.refinements
            .nonfallthrough_loop_assignment_snapshots
            .push(BTreeMap::new());
        if then_always_exits {
            self.refinements
                .nonfallthrough_loop_assignment_snapshots
                .push(BTreeMap::new());
        }
        self.visit_stat(then_scope, then_body);
        if then_always_exits {
            self.restore_nonfallthrough_loop_assignments();
        }
        let then_after_refinements = self.refinements.locals.pop().unwrap_or_default();
        let then_assignment_refinements =
            Self::branch_assignment_refinements(&then_refinements, &then_after_refinements);
        let then_assignment_snapshot = self
            .refinements
            .nonfallthrough_loop_assignment_snapshots
            .pop()
            .unwrap_or_default();
        self.restore_assignment_snapshot(then_assignment_snapshot);
        let else_after_refinements = if let Some(else_body) = else_body {
            self.refinements.locals = base_refined_locals.clone();
            let else_scope = self.enter_child(scope);
            self.refinements.locals.push(else_refinements.clone());
            self.refinements
                .nonfallthrough_loop_assignment_snapshots
                .push(BTreeMap::new());
            if else_always_exits {
                self.refinements
                    .nonfallthrough_loop_assignment_snapshots
                    .push(BTreeMap::new());
            }
            self.visit_stat(else_scope, else_body);
            if else_always_exits {
                self.restore_nonfallthrough_loop_assignments();
            }
            let refinements = self.refinements.locals.pop().unwrap_or_default();
            let assignment_snapshot = self
                .refinements
                .nonfallthrough_loop_assignment_snapshots
                .pop()
                .unwrap_or_default();
            self.restore_assignment_snapshot(assignment_snapshot);
            self.refinements.locals = base_refined_locals;
            refinements
        } else {
            self.refinements.locals = base_refined_locals;
            else_refinements.clone()
        };
        let else_assignment_refinements =
            Self::branch_assignment_refinements(&else_refinements, &else_after_refinements);
        let assignment_refinement_keys = Self::merged_refinement_keys([
            &then_assignment_refinements,
            &else_assignment_refinements,
        ]);
        let then_exits = self.stat_exits(then_body);
        let else_exits = else_body.is_some_and(|else_body| self.stat_exits(else_body));
        let joined_refinements = if then_exits && !else_exits {
            else_after_refinements
        } else if else_exits {
            then_after_refinements
        } else {
            let then_join_refinements =
                Self::refinements_for_keys(&then_after_refinements, &assignment_refinement_keys);
            let else_join_refinements =
                Self::refinements_for_keys(&else_after_refinements, &assignment_refinement_keys);
            self.merge_branch_refinements([then_join_refinements, else_join_refinements])
        };
        self.record_query_refinement_types(&joined_refinements);
        self.merge_current_refinements(joined_refinements);
    }

    /// Visits a loop body with the shared loop bookkeeping: an
    /// always-exiting body brackets the non-fallthrough assignment
    /// snapshot, and `loop_depth` (plus `repeat_guaranteed_body_depth`
    /// when the body is guaranteed to run, as in `repeat`) spans the walk.
    fn visit_loop_body(&mut self, body_scope: ScopeId, body: &Stat, guaranteed_first_pass: bool) {
        let body_always_exits = self.stat_always_exits(body);
        if body_always_exits {
            self.refinements
                .nonfallthrough_loop_assignment_snapshots
                .push(BTreeMap::new());
        }
        self.loop_depth += 1;
        if guaranteed_first_pass {
            self.repeat_guaranteed_body_depth += 1;
        }
        self.visit_stat(body_scope, body);
        if guaranteed_first_pass {
            self.repeat_guaranteed_body_depth -= 1;
        }
        self.loop_depth -= 1;
        if body_always_exits {
            self.restore_nonfallthrough_loop_assignments();
        }
    }

    fn stat_while(&mut self, scope: ScopeId, condition: &Expr, body: &Stat) {
        self.expr_type_in_refinement_context(scope, condition);
        let refinements = self.truthy_refinements(condition);
        let body_scope = self.enter_child(scope);
        self.refinements.locals.push(refinements);
        self.visit_loop_body(body_scope, body, false);
        self.refinements.locals.pop();
    }

    fn stat_repeat(&mut self, scope: ScopeId, condition: &Expr, body: &Stat) {
        let body_scope = self.enter_child(scope);
        self.visit_loop_body(body_scope, body, true);
        self.expr_type_in_refinement_context(body_scope, condition);
    }

    fn stat_for(
        &mut self,
        scope: ScopeId,
        var: &Local,
        from: &Expr,
        to: &Expr,
        step: Option<&Expr>,
        body: &Stat,
    ) {
        let number = self.arena.primitives().number;
        for expr in [Some(from), Some(to), step].into_iter().flatten() {
            let ty = self.expr_type(scope, expr);
            if !self.is_dynamic(ty) {
                self.generated.constraints.push(Constraint::subtype(
                    ty,
                    number,
                    expr.location().map(DiagnosticLocation::from),
                ));
            }
        }
        let body_scope = self.enter_child(scope);
        let var_ty = self.local_type(var);
        if let Some(annotation) = &var.luau_type {
            let annotation_ty = self.lower_type(scope, annotation);
            self.expect_type(var.location, var_ty, annotation_ty);
            self.bind_free_to(var_ty, annotation_ty);
            self.generated
                .constraints
                .push(Constraint::subtype_default_location(
                    number,
                    annotation_ty,
                    var.location.map(DiagnosticLocation::from),
                ));
        } else {
            self.generated
                .constraints
                .push(Constraint::unify(var_ty, number));
        }
        self.visit_loop_body(body_scope, body, false);
    }

    fn local_initializer_refines_value(values: &[Expr], index: usize) -> bool {
        let value = if values.len() == 1 {
            values.first()
        } else {
            values.get(index)
        };
        value.is_some_and(|value| matches!(ungroup_expr(value), Expr::Call { .. }))
    }
    fn local_initializer_relaxes_nil_guard(&self, values: &[Expr], index: usize) -> bool {
        let value = if values.len() == 1 {
            values.first()
        } else {
            values.get(index)
        };
        value.is_some_and(|value| self.expr_relaxes_nil_guard(value))
    }
    fn expr_relaxes_nil_guard(&self, expr: &Expr) -> bool {
        match ungroup_expr(expr) {
            Expr::Call { .. } => true,
            Expr::Local { local, .. } => self
                .nil_tracking
                .guard_relaxes_to_nil_locals
                .contains(&local.id),
            Expr::IndexName { expr, .. } | Expr::IndexExpr { expr, .. } => {
                self.index_base_relaxes_nil_guard(expr)
            }
            Expr::TypeAssertion { expr, .. } => self.expr_relaxes_nil_guard(expr),
            _ => false,
        }
    }
    fn index_base_relaxes_nil_guard(&self, expr: &Expr) -> bool {
        match ungroup_expr(expr) {
            Expr::Local { local, .. } => {
                self.nil_tracking
                    .guard_relaxes_to_nil_locals
                    .contains(&local.id)
                    || self
                        .input
                        .scopes
                        .lookup_local_id(local.id)
                        .is_some_and(|binding| binding.kind == ValueBindingKind::FunctionParameter)
            }
            Expr::IndexName { expr, .. } | Expr::IndexExpr { expr, .. } => {
                self.index_base_relaxes_nil_guard(expr)
            }
            Expr::TypeAssertion { expr, .. } => self.index_base_relaxes_nil_guard(expr),
            _ => false,
        }
    }
    fn eager_annotation_value_location(value: &Expr) -> Option<Location> {
        match ungroup_expr(value) {
            Expr::Nil { location, .. }
            | Expr::Bool { location, .. }
            | Expr::Number { location, .. }
            | Expr::Integer { location, .. }
            | Expr::String { location, .. } => *location,
            _ => None,
        }
    }
    fn eager_local_annotation_mismatch(
        &self,
        value_ty: TypeId,
        annotation_ty: TypeId,
        location: Option<Location>,
        allow_non_function_surfaces: bool,
    ) -> Option<Diagnostic> {
        if !(allow_non_function_surfaces
            && self.is_non_function_eager_surface(value_ty)
            && self.is_non_function_eager_surface(annotation_ty)
            || self.is_singleton_annotation_surface(value_ty)
                && self.is_singleton_annotation_surface(annotation_ty)
            || self.is_top_function_annotation_pair(value_ty, annotation_ty))
        {
            return None;
        }
        let error = Subtyper::new(self.arena)
            .is_subtype(value_ty, annotation_ty)
            .err()?;
        let mut diagnostic =
            ConstraintSolveError::Subtype(error).into_diagnostic_with_arena(Some(self.arena));
        diagnostic.primary_location =
            location.map_or_else(DiagnosticLocation::missing, DiagnosticLocation::from);
        Some(diagnostic)
    }
    fn is_singleton_annotation_surface(&self, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Singleton(_) => true,
            TypeKind::Primitive(crate::types::PrimitiveType::Nil) => true,
            TypeKind::Union(types) => types
                .iter()
                .all(|ty| self.is_singleton_annotation_surface(*ty)),
            _ => false,
        }
    }

    fn is_top_function_annotation_pair(&self, left: TypeId, right: TypeId) -> bool {
        self.is_top_function_surface(left) && self.is_non_function_eager_surface(right)
            || self.is_non_function_eager_surface(left) && self.is_top_function_surface(right)
    }

    fn is_top_function_surface(&self, ty: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(ty)),
            TypeKind::Function(function) if is_top_function_type(self.arena, function)
        )
    }

    fn is_non_function_eager_surface(&self, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Singleton(_) | TypeKind::Primitive(_) => true,
            TypeKind::Extern { .. } => true,
            TypeKind::Table(table) => {
                table.indexer.is_none() && table.instantiated_type_params.is_empty()
            }
            TypeKind::Union(types) => types
                .iter()
                .all(|ty| self.is_non_function_eager_surface(*ty)),
            _ => false,
        }
    }
    fn is_generic_pack_overload_surface(&self, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Intersection(options) => options.iter().any(|option| {
                let TypeKind::Function(function) = self.arena.get(self.arena.follow(*option))
                else {
                    return false;
                };
                let mut seen_packs = BTreeSet::new();
                self.pack_mentions_generic_pack(function.arguments, &mut seen_packs)
                    || self.pack_mentions_generic_pack(function.returns, &mut seen_packs)
            }),
            _ => false,
        }
    }

    fn is_intersection_returning_overload_surface(
        &self,
        value_ty: TypeId,
        annotation_ty: TypeId,
    ) -> bool {
        if !matches!(
            self.arena.get(self.arena.follow(annotation_ty)),
            TypeKind::Function(_)
        ) {
            return false;
        }

        match self.arena.get(self.arena.follow(value_ty)) {
            TypeKind::Intersection(options) if options.len() > 1 => options.iter().any(|option| {
                let TypeKind::Function(function) = self.arena.get(self.arena.follow(*option))
                else {
                    return false;
                };
                self.pack_mentions_direct_intersection(function.returns, &mut BTreeSet::new())
            }),
            _ => false,
        }
    }

    fn is_extern_indexer_annotation_surface(
        &self,
        value_ty: TypeId,
        annotation_ty: TypeId,
    ) -> bool {
        matches!(
            self.arena.get(self.arena.follow(value_ty)),
            TypeKind::Extern { .. }
        ) && matches!(
            self.arena.get(self.arena.follow(annotation_ty)),
            TypeKind::Table(table) if table.indexer.is_some()
        )
    }

    fn pack_mentions_generic_pack(
        &self,
        pack: crate::types::TypePackId,
        seen_packs: &mut BTreeSet<crate::types::TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack) {
            TypePackKind::Generic(_) => true,
            TypePackKind::List { tail, .. } => {
                tail.is_some_and(|tail| self.pack_mentions_generic_pack(tail, seen_packs))
            }
            TypePackKind::Bound(tail) => self.pack_mentions_generic_pack(*tail, seen_packs),
            TypePackKind::Variadic { .. } | TypePackKind::Free { .. } | TypePackKind::Error => {
                false
            }
        }
    }
    pub(crate) fn assign_lvalue(&mut self, scope: ScopeId, var: &Expr, value: &Expr) {
        // A function literal written to a property with a declared function
        // type is a function definition in assignment clothing: route it
        // through the same declared-index path as `function t.f(x)` so the
        // body is checked against the declared signature and the property
        // keeps its declared type.
        if matches!(value, Expr::Function { .. })
            && is_plain_index_function_name(var)
            && let Some((expected, expected_from_intersection)) =
                self.plain_index_function_expected_type(scope, var)
        {
            self.assign_function_to_declared_index(
                scope,
                var,
                value,
                expected,
                expected_from_intersection,
            );
            return;
        }
        match var {
            Expr::IndexName {
                location,
                expr: base,
                index,
                ..
            } => {
                let expr_ty = self.dfg_type_for_expr(var);
                let base_ty = self.expr_type(scope, base);
                let expected_value_ty = self.direct_property_write_type(base_ty, index.as_str());
                let value_ty = self.expr_type_with_expected(scope, value, expected_value_ty);
                let stored_ty =
                    widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
                if self.is_dynamic(base_ty) {
                    self.bind_actual(*location, var.syntax_id(), expr_ty, self.primitives().any);
                    return;
                }
                if self.is_never_type(base_ty) {
                    self.bind_actual(*location, var.syntax_id(), expr_ty, self.primitives().never);
                    return;
                }
                if self.report_eager_missing_property_write(
                    *location,
                    base_ty,
                    index.as_str(),
                    value_ty,
                ) {
                    self.record_actual(*location, var.syntax_id(), expr_ty);
                    return;
                }
                if self.report_eager_property_write_mismatch(
                    *location,
                    value.location(),
                    base_ty,
                    index.as_str(),
                    value_ty,
                ) {
                    self.record_actual(*location, var.syntax_id(), expr_ty);
                    return;
                }
                self.generated
                    .constraints
                    .push(Constraint::unify(expr_ty, value_ty));
                self.record_unsealed_property_write(base_ty, index.as_str(), stored_ty);
                self.generated.constraints.push(Constraint::write_property(
                    base_ty,
                    index.as_str().to_owned(),
                    value_ty,
                    location.map(DiagnosticLocation::from),
                ));
                self.record_actual(*location, var.syntax_id(), expr_ty);
            }
            Expr::IndexExpr {
                location,
                expr: base,
                index,
                ..
            } => {
                let expr_ty = self.dfg_type_for_expr(var);
                let base_ty = self.expr_type(scope, base);
                let index_ty = self.expr_type(scope, index);
                self.record_contextual_index_key_query(base_ty, index, index_ty);
                let stored_key = if matches!(index.as_ref(), Expr::String { .. }) {
                    index_ty
                } else {
                    widened_table_literal_value_type(self.arena, index).unwrap_or(index_ty)
                };
                let expected_value_ty = if matches!(value, Expr::Nil { .. }) {
                    None
                } else {
                    self.direct_indexer_write_type(base_ty, stored_key)
                };
                let value_ty = self.expr_type_with_expected(scope, value, expected_value_ty);
                let stored_value =
                    widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
                if self.is_dynamic(base_ty) {
                    self.bind_actual(*location, var.syntax_id(), expr_ty, self.primitives().any);
                    return;
                }
                if self.is_never_type(base_ty) {
                    self.bind_actual(*location, var.syntax_id(), expr_ty, self.primitives().never);
                    return;
                }
                self.generated
                    .constraints
                    .push(Constraint::unify(expr_ty, value_ty));
                self.bind_function_parameter_indexer_expected_type(base, stored_key, value_ty);
                if self.index_expr_base_is_captured_upvalue(scope, base) {
                    self.record_unsealed_indexer_write(base_ty, stored_key, stored_value);
                }
                self.generated.constraints.push(Constraint::write_indexer(
                    base_ty,
                    stored_key,
                    value_ty,
                    location.map(DiagnosticLocation::from),
                ));
                self.record_actual(*location, var.syntax_id(), expr_ty);
            }
            Expr::Local { local, .. } => {
                let var_ty = self
                    .input
                    .dfg
                    .local(local.id)
                    .map(|def| self.input.dfg.get(def).ty)
                    .unwrap_or_else(|| self.recovery_type_at(local.location, "missing local def"));
                if self.local_is_captured_upvalue(scope, local.id) {
                    let expected_ty = self.widen_mutable_literal_type(var_ty);
                    let expected_ty = (self.input.mode != AnalysisMode::Nonstrict
                        || local.luau_type.is_some())
                    .then_some(expected_ty);
                    let value_ty = self.expr_type_with_expected(scope, value, expected_ty);
                    let stored_ty =
                        widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
                    if self.input.mode == AnalysisMode::Nonstrict && local.luau_type.is_none() {
                        self.snapshot_nonfallthrough_loop_assignment(var_ty);
                        self.assign_unannotated_local_type(local.id, var_ty, stored_ty, false);
                    } else {
                        self.assign_captured_local_type(local.id, var_ty, value_ty, stored_ty);
                    }
                    return;
                }
                if self.try_assign_local_type_from_value(scope, local, var_ty, value) {
                    return;
                }
                let value_location = Self::eager_annotation_value_location(value);
                let value_ty = if value_location.is_some() {
                    self.expr_type(scope, value)
                } else {
                    self.expr_type_with_expected(scope, value, Some(var_ty))
                };
                let stored_ty =
                    widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
                if self.input.mode == AnalysisMode::Nonstrict && local.luau_type.is_none() {
                    self.snapshot_nonfallthrough_loop_assignment(var_ty);
                    self.assign_unannotated_local_type(local.id, var_ty, stored_ty, true);
                } else {
                    self.assign_annotated_local_type(
                        local,
                        var_ty,
                        value_ty,
                        stored_ty,
                        value_location,
                    );
                }
                self.refine_assigned_local(local.id, var_ty, stored_ty);
            }
            Expr::Global { location, name, .. } => {
                let expr_ty = self.dfg_type_for_expr(var);
                let global_ty = self
                    .input
                    .scopes
                    .lookup_global(scope, name.as_str())
                    .and_then(|binding| binding.ty)
                    .or_else(|| self.generated.global_defs.get(name.as_str()).copied());
                let suppress_self_read = self.input.mode != AnalysisMode::Strict
                    && global_ty.is_none()
                    && expr_reads_global(value, name.as_str());
                if suppress_self_read {
                    self.report_unknown_symbol(
                        var.syntax_id(),
                        name.as_str(),
                        DiagnosticLocation::from_opt(*location),
                    );
                }
                let value_ty = if suppress_self_read {
                    self.with_suppressed_unknown_global(name.as_str(), |this| {
                        this.expr_type(scope, value)
                    })
                } else {
                    self.expr_type(scope, value)
                };
                let stored_ty =
                    widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
                match global_ty {
                    Some(global_ty) => {
                        self.bind_actual(*location, var.syntax_id(), expr_ty, global_ty);
                        if !self.is_dynamic(value_ty) && !self.is_never_type(global_ty) {
                            self.generated.constraints.push(Constraint::subtype(
                                value_ty,
                                global_ty,
                                location.map(DiagnosticLocation::from),
                            ));
                        }
                        self.merge_current_refinements(RefinementMap::from([(
                            RefinementKey::Symbol(Symbol::Global(name.as_str().to_owned())),
                            self.widen_mutable_literal_type(stored_ty),
                        )]));
                    }
                    None if self.input.mode != AnalysisMode::Strict => {
                        self.generated
                            .global_defs
                            .insert(name.as_str().to_owned(), self.arena.follow(stored_ty));
                        self.bind_actual(*location, var.syntax_id(), expr_ty, stored_ty);
                    }
                    None => {
                        self.report_unknown_symbol(
                            var.syntax_id(),
                            name.as_str(),
                            DiagnosticLocation::from_opt(*location),
                        );
                        self.generated
                            .global_defs
                            .insert(name.as_str().to_owned(), self.arena.follow(stored_ty));
                        self.bind_actual(*location, var.syntax_id(), expr_ty, stored_ty);
                        self.merge_current_refinements(RefinementMap::from([(
                            RefinementKey::Symbol(Symbol::Global(name.as_str().to_owned())),
                            self.widen_mutable_literal_type(stored_ty),
                        )]));
                    }
                }
            }
            Expr::Group { expr, .. } => self.assign_lvalue(scope, expr, value),
            _ => {
                let var_ty = self.expr_type(scope, var);
                let value_ty = self.expr_type_with_expected(scope, value, Some(var_ty));
                self.bind_free_to(var_ty, value_ty);
                if !self.is_dynamic(value_ty) {
                    self.generated.constraints.push(Constraint::subtype(
                        value_ty,
                        var_ty,
                        var.location().map(DiagnosticLocation::from),
                    ));
                }
            }
        }
    }

    fn index_expr_base_is_captured_upvalue(&self, scope: ScopeId, base: &Expr) -> bool {
        matches!(
            base,
            Expr::Local { local, .. } if self.local_is_captured_upvalue(scope, local.id)
        )
    }
    fn report_eager_property_write_mismatch(
        &mut self,
        location: Option<Location>,
        value_location: Option<Location>,
        table: TypeId,
        property: &str,
        value: TypeId,
    ) -> bool {
        let Some(write_ty) = self.direct_dual_property_write_type(table, property) else {
            return false;
        };
        let value = self.arena.follow(value);
        if self.is_dynamic(value) {
            return false;
        }
        let Some(error) = Subtyper::new(self.arena).is_subtype(value, write_ty).err() else {
            return false;
        };
        let mut diagnostic =
            ConstraintSolveError::Subtype(error).into_diagnostic_with_arena(Some(self.arena));
        // Upstream's "Expected this to be 'T', but got 'U'" points at the
        // assigned *value* (`fh.real_property = nil` → the `nil`), not the write
        // target; prefer the value's span when the assignment exposes it.
        diagnostic.primary_location = value_location
            .or(location)
            .map_or_else(DiagnosticLocation::missing, DiagnosticLocation::from);
        self.generated.deferred_diagnostics.push(diagnostic);
        true
    }

    fn report_eager_missing_property_write(
        &mut self,
        location: Option<Location>,
        table: TypeId,
        property: &str,
        value: TypeId,
    ) -> bool {
        let Some(table) = self.sealed_table_missing_write_property(table, property) else {
            return false;
        };
        let mut expected = TableType::new(TableState::Sealed);
        expected
            .properties
            .insert(property.to_owned(), TableProperty::new(value));
        let expected = self.arena.alloc(TypeKind::Table(expected));
        let error = SubtypeError {
            kind: SubtypeErrorKind::MissingProperty,
            path: TypePath::new().push(TypePathComponent::write_property(property)),
            sub: SubtypeTarget::Type(table),
            sup: SubtypeTarget::Type(expected),
        };
        let suppression = Subtyper::new(self.arena).suppression(table, expected);
        let mut diagnostic = ConstraintSolveError::SubtypeWithMetadata {
            error: Box::new(error),
            sub: SubtypeTarget::Type(table),
            sup: SubtypeTarget::Type(expected),
            suppression,
        }
        .into_diagnostic_with_arena(Some(&*self.arena));
        diagnostic.primary_location =
            location.map_or_else(DiagnosticLocation::missing, DiagnosticLocation::from);
        self.generated.deferred_diagnostics.push(diagnostic);
        true
    }

    fn sealed_table_missing_write_property(&self, table: TypeId, property: &str) -> Option<TypeId> {
        let table = self.arena.follow(table);
        match self.arena.get(table) {
            TypeKind::Table(table_type)
                if table_type.state == TableState::Sealed
                    && table_type.name.is_none()
                    && table_type.instantiated_type_params.is_empty()
                    && table_type.indexer.is_none()
                    && !table_type.properties.contains_key(property) =>
            {
                Some(table)
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.sealed_table_missing_write_property(*base_table, property),
            _ => None,
        }
    }

    fn direct_dual_property_write_type(&self, table: TypeId, property: &str) -> Option<TypeId> {
        match self.arena.get(self.arena.follow(table)) {
            TypeKind::Table(table) => table.properties.get(property)?.write_ty,
            // A sealed class property's declared type is its write type; reject
            // an ill-typed `obj.prop = nil` eagerly (at the value's span).
            TypeKind::Extern { properties, .. } => {
                let property = properties.get(property)?;
                Some(property.write_ty.unwrap_or(property.ty))
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.direct_dual_property_write_type(*base_table, property),
            _ => None,
        }
    }

    fn direct_property_write_type(&self, table: TypeId, property: &str) -> Option<TypeId> {
        match self.arena.get(self.arena.follow(table)) {
            TypeKind::Table(table) => table
                .properties
                .get(property)
                .map(TableProperty::write_type),
            TypeKind::Extern { properties, .. } => {
                properties.get(property).map(TableProperty::write_type)
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.direct_property_write_type(*base_table, property),
            _ => None,
        }
    }

    fn direct_indexer_write_type(&self, table: TypeId, key: TypeId) -> Option<TypeId> {
        match self.arena.get(self.arena.follow(table)) {
            TypeKind::Table(table) => table.indexer.as_ref().and_then(|indexer| {
                Subtyper::new(self.arena)
                    .is_subtype(key, indexer.key)
                    .is_ok()
                    .then_some(indexer.value)
            }),
            TypeKind::Extern { indexer, .. } => indexer.as_ref().and_then(|indexer| {
                Subtyper::new(self.arena)
                    .is_subtype(key, indexer.key)
                    .is_ok()
                    .then_some(indexer.value)
            }),
            TypeKind::Metatable {
                table: base_table, ..
            } => self.direct_indexer_write_type(*base_table, key),
            _ => None,
        }
    }

    pub(crate) fn assign_known_type_to_lvalue(
        &mut self,
        scope: ScopeId,
        var: &Expr,
        value_ty: TypeId,
    ) {
        match var {
            Expr::IndexName {
                location,
                expr: base,
                index,
                ..
            } => {
                let expr_ty = self.dfg_type_for_expr(var);
                let base_ty = self.expr_type(scope, base);
                if self.is_dynamic(base_ty) {
                    self.bind_actual(*location, var.syntax_id(), expr_ty, self.primitives().any);
                    return;
                }
                if self.is_never_type(base_ty) {
                    self.bind_actual(*location, var.syntax_id(), expr_ty, self.primitives().never);
                    return;
                }
                if self.report_eager_missing_property_write(
                    *location,
                    base_ty,
                    index.as_str(),
                    value_ty,
                ) {
                    self.record_actual(*location, var.syntax_id(), expr_ty);
                    return;
                }
                if self.report_eager_property_write_mismatch(
                    *location,
                    None,
                    base_ty,
                    index.as_str(),
                    value_ty,
                ) {
                    self.record_actual(*location, var.syntax_id(), expr_ty);
                    return;
                }
                if self.report_known_non_table_property_write(base_ty, *location) {
                    self.record_actual(*location, var.syntax_id(), expr_ty);
                    return;
                }
                self.generated
                    .constraints
                    .push(Constraint::unify(expr_ty, value_ty));
                self.record_unsealed_property_write(base_ty, index.as_str(), value_ty);
                self.generated.constraints.push(Constraint::write_property(
                    base_ty,
                    index.as_str().to_owned(),
                    value_ty,
                    location.map(DiagnosticLocation::from),
                ));
                self.record_actual(*location, var.syntax_id(), expr_ty);
            }
            Expr::IndexExpr {
                location,
                expr: base,
                index,
                ..
            } => {
                let expr_ty = self.dfg_type_for_expr(var);
                let base_ty = self.expr_type(scope, base);
                let index_ty = self.expr_type(scope, index);
                self.record_contextual_index_key_query(base_ty, index, index_ty);
                if self.is_dynamic(base_ty) {
                    self.bind_actual(*location, var.syntax_id(), expr_ty, self.primitives().any);
                    return;
                }
                if self.is_never_type(base_ty) {
                    self.bind_actual(*location, var.syntax_id(), expr_ty, self.primitives().never);
                    return;
                }
                self.generated
                    .constraints
                    .push(Constraint::unify(expr_ty, value_ty));
                self.bind_function_parameter_indexer_expected_type(base, index_ty, value_ty);
                self.generated.constraints.push(Constraint::write_indexer(
                    base_ty,
                    index_ty,
                    value_ty,
                    location.map(DiagnosticLocation::from),
                ));
                self.record_actual(*location, var.syntax_id(), expr_ty);
            }
            Expr::Global { location, name, .. } => {
                let expr_ty = self.dfg_type_for_expr(var);
                let stored_ty = self.widen_mutable_literal_type(value_ty);
                let global_ty = self
                    .input
                    .scopes
                    .lookup_global(scope, name.as_str())
                    .and_then(|binding| binding.ty)
                    .or_else(|| self.generated.global_defs.get(name.as_str()).copied());
                match global_ty {
                    Some(global_ty) => {
                        self.bind_actual(*location, var.syntax_id(), expr_ty, global_ty);
                        if !self.is_dynamic(value_ty) && !self.is_never_type(global_ty) {
                            self.generated.constraints.push(Constraint::subtype(
                                value_ty,
                                global_ty,
                                location.map(DiagnosticLocation::from),
                            ));
                        }
                        self.merge_current_refinements(RefinementMap::from([(
                            RefinementKey::Symbol(Symbol::Global(name.as_str().to_owned())),
                            self.widen_mutable_literal_type(stored_ty),
                        )]));
                    }
                    None if self.input.mode != AnalysisMode::Strict => {
                        self.generated
                            .global_defs
                            .insert(name.as_str().to_owned(), self.arena.follow(stored_ty));
                        self.bind_actual(*location, var.syntax_id(), expr_ty, stored_ty);
                    }
                    None => {
                        self.report_unknown_symbol(
                            var.syntax_id(),
                            name.as_str(),
                            DiagnosticLocation::from_opt(*location),
                        );
                        self.generated
                            .global_defs
                            .insert(name.as_str().to_owned(), self.arena.follow(stored_ty));
                        self.bind_actual(*location, var.syntax_id(), expr_ty, stored_ty);
                        self.merge_current_refinements(RefinementMap::from([(
                            RefinementKey::Symbol(Symbol::Global(name.as_str().to_owned())),
                            self.widen_mutable_literal_type(stored_ty),
                        )]));
                    }
                }
            }
            Expr::Group { expr, .. } => self.assign_known_type_to_lvalue(scope, expr, value_ty),
            _ => {
                let var_ty = self.lvalue_type(scope, var);
                self.generated
                    .constraints
                    .push(Constraint::unify(var_ty, value_ty));
            }
        }
    }
    fn lvalue_is_simple_binding(&self, var: &Expr) -> bool {
        match var {
            Expr::Local { .. } | Expr::Global { .. } => true,
            Expr::Group { expr, .. } => self.lvalue_is_simple_binding(expr),
            _ => false,
        }
    }

    fn pack_mentions_direct_intersection(
        &self,
        pack: crate::types::TypePackId,
        seen_packs: &mut BTreeSet<crate::types::TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack) {
            TypePackKind::List { types, tail } => {
                types.iter().any(|ty| {
                    matches!(
                        self.arena.get(self.arena.follow(*ty)),
                        TypeKind::Intersection(_)
                    )
                }) || tail
                    .is_some_and(|tail| self.pack_mentions_direct_intersection(tail, seen_packs))
            }
            TypePackKind::Variadic { ty } => {
                matches!(
                    self.arena.get(self.arena.follow(*ty)),
                    TypeKind::Intersection(_)
                )
            }
            TypePackKind::Bound(tail) => self.pack_mentions_direct_intersection(*tail, seen_packs),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }
    pub(crate) fn write_compound_lvalue(&mut self, scope: ScopeId, var: &Expr, value_ty: TypeId) {
        match var {
            Expr::IndexName {
                location,
                expr: base,
                index,
                ..
            } => {
                let table = self.expr_type(scope, base);
                if self.is_dynamic(table) || self.is_never_type(table) {
                    return;
                }
                self.generated.constraints.push(Constraint::write_property(
                    table,
                    index.as_str().to_owned(),
                    value_ty,
                    location.map(DiagnosticLocation::from),
                ));
            }
            Expr::IndexExpr {
                location,
                expr: base,
                index,
                ..
            } => {
                let table = self.expr_type(scope, base);
                if self.is_dynamic(table) || self.is_never_type(table) {
                    return;
                }
                let key = self.expr_type(scope, index);
                self.generated.constraints.push(Constraint::write_indexer(
                    table,
                    key,
                    value_ty,
                    location.map(DiagnosticLocation::from),
                ));
            }
            Expr::Group { expr, .. } => self.write_compound_lvalue(scope, expr, value_ty),
            Expr::Local { .. } | Expr::Global { .. } | Expr::Error { .. } => {}
            _ => {}
        }
    }
    pub(crate) fn try_assign_local_type_from_value(
        &mut self,
        scope: ScopeId,
        local: &LocalRef,
        var_ty: TypeId,
        value: &Expr,
    ) -> bool {
        if self.rebind_nil_initialized_local_assignment(local.id, var_ty, scope, value) {
            return true;
        }
        if self.should_bind_loop_assignment_as_optional(local.id, var_ty) {
            let value_ty = self.expr_type(scope, value);
            let stored_ty = widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
            self.bind_loop_assignment_as_optional(var_ty, stored_ty);
            self.refine_current_local(local.id, stored_ty);
            return true;
        }
        if self.should_widen_loop_assignment(local, var_ty) {
            let value_ty = self.expr_type(scope, value);
            let stored_ty = widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
            if self.nil_tracking.initialized_locals.contains(&local.id) {
                self.record_started_as_nil_query_type(local.id, stored_ty);
                self.widen_nil_initialized_loop_assignment(var_ty, stored_ty);
            } else {
                self.widen_loop_assignment(var_ty, stored_ty);
            }
            self.refine_current_local(local.id, stored_ty);
            return true;
        }
        let restores_nil_initialized_local =
            self.nil_tracking.initialized_locals.contains(&local.id)
                && matches!(self.arena.get(var_ty), TypeKind::Free(_))
                && !self
                    .refinements
                    .nonfallthrough_loop_assignment_snapshots
                    .is_empty();
        if !restores_nil_initialized_local && local.luau_type.is_some() {
            return false;
        }
        let value_ty = self.expr_type(scope, value);
        let stored_ty = widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
        if restores_nil_initialized_local {
            self.snapshot_nonfallthrough_loop_assignment_with(
                var_ty,
                TypeKind::Bound(self.primitives().nil),
            );
        } else {
            self.snapshot_nonfallthrough_loop_assignment(var_ty);
        }
        if restores_nil_initialized_local {
            self.bind_free_to(var_ty, stored_ty);
        } else {
            self.assign_unannotated_local_type(local.id, var_ty, stored_ty, true);
        }
        true
    }
    pub(crate) fn assign_local_type(
        &mut self,
        scope: ScopeId,
        local: &LocalRef,
        local_ty: TypeId,
        value_ty: TypeId,
        value_location: Option<Location>,
    ) {
        if self.local_is_captured_upvalue(scope, local.id) {
            self.assign_captured_local_type(local.id, local_ty, value_ty, value_ty);
        } else if self.should_bind_loop_assignment_as_optional(local.id, local_ty) {
            self.bind_loop_assignment_as_optional(local_ty, value_ty);
            self.refine_current_local(local.id, value_ty);
        } else if self.should_widen_loop_assignment(local, local_ty) {
            if self.nil_tracking.initialized_locals.contains(&local.id) {
                self.record_started_as_nil_query_type(local.id, value_ty);
                self.widen_nil_initialized_loop_assignment(local_ty, value_ty);
            } else {
                self.widen_loop_assignment(local_ty, value_ty);
            }
            self.refine_current_local(local.id, value_ty);
        } else if local.luau_type.is_some() {
            self.assign_annotated_local_type(local, local_ty, value_ty, value_ty, value_location);
            self.refine_assigned_local(local.id, local_ty, value_ty);
        } else {
            self.snapshot_nonfallthrough_loop_assignment(local_ty);
            self.assign_unannotated_local_type(local.id, local_ty, value_ty, true);
        }
    }
    pub(crate) fn assign_captured_local_type(
        &mut self,
        local_id: LocalId,
        local_ty: TypeId,
        value_ty: TypeId,
        stored_ty: TypeId,
    ) {
        if self.assign_guaranteed_repeat_upvalue_type(local_id, local_ty, stored_ty) {
            return;
        }
        let assignable_local_ty = self.widen_mutable_literal_type(local_ty);
        let stored_ty = self.widen_mutable_literal_type(stored_ty);
        if matches!(
            self.arena.get(self.arena.follow(local_ty)),
            TypeKind::Free(_)
        ) && self.is_unbound_free(value_ty)
        {
            let any = self.primitives().any;
            self.bind_free_to(local_ty, any);
            self.generated
                .query_local_types
                .insert(local_id, self.primitives().unknown);
            self.merge_current_refinements(RefinementMap::from([(
                RefinementKey::Symbol(Symbol::Local(local_id)),
                any,
            )]));
            return;
        }
        if self.is_dynamic(stored_ty)
            && matches!(
                self.arena.get(self.arena.follow(local_ty)),
                TypeKind::Free(_)
            )
        {
            self.bind_free_to(local_ty, stored_ty);
        } else if !self.is_dynamic(value_ty) {
            self.generated.constraints.push(Constraint::subtype(
                value_ty,
                assignable_local_ty,
                None,
            ));
        }
        self.merge_current_refinements(RefinementMap::from([(
            RefinementKey::Symbol(Symbol::Local(local_id)),
            stored_ty,
        )]));
    }
    fn is_unbound_free(&self, ty: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(ty)),
            TypeKind::Free(variable)
                if variable.lower_bound.is_none() && variable.upper_bound.is_none()
        )
    }
    pub(crate) fn assign_guaranteed_repeat_upvalue_type(
        &mut self,
        local_id: LocalId,
        local_ty: TypeId,
        stored_ty: TypeId,
    ) -> bool {
        if self.loop_depth == 0
            || self.loop_assignment_may_be_skipped()
            || self.local_surface.annotated_locals.contains(&local_id)
        {
            return false;
        }

        let stored_ty = self.widen_mutable_literal_type(stored_ty);
        self.snapshot_nonfallthrough_loop_assignment(local_ty);
        if matches!(
            self.arena.get(self.arena.follow(local_ty)),
            TypeKind::Free(_)
        ) {
            self.bind_free_to(local_ty, stored_ty);
        } else {
            self.arena.replace(local_ty, TypeKind::Bound(stored_ty));
        }
        self.merge_current_refinements(RefinementMap::from([(
            RefinementKey::Symbol(Symbol::Local(local_id)),
            stored_ty,
        )]));
        true
    }
    pub(crate) fn assign_annotated_local_type(
        &mut self,
        local: &LocalRef,
        local_ty: TypeId,
        value_ty: TypeId,
        stored_ty: TypeId,
        value_location: Option<Location>,
    ) {
        self.snapshot_nonfallthrough_loop_assignment(local_ty);
        self.bind_free_to(local_ty, stored_ty);
        if !self.is_dynamic(value_ty) {
            if let Some(diagnostic) = self.eager_local_annotation_mismatch(
                value_ty,
                local_ty,
                value_location.or(local.location),
                value_location.is_some(),
            ) {
                self.generated.deferred_diagnostics.push(diagnostic);
            } else {
                self.push_annotation_subtype_constraint(
                    value_ty,
                    local_ty,
                    value_location.or(local.location),
                );
            }
        }
    }
    fn refine_assigned_local(&mut self, local_id: LocalId, local_ty: TypeId, value_ty: TypeId) {
        let refined_ty = if let Some(refined_ty) =
            self.error_suppressing_assignment_refinement(local_ty, value_ty)
        {
            refined_ty
        } else if !self.is_dynamic(local_ty) {
            self.widen_mutable_literal_type(value_ty)
        } else {
            return;
        };
        self.merge_current_refinements(RefinementMap::from([(
            RefinementKey::Symbol(Symbol::Local(local_id)),
            refined_ty,
        )]));
    }
    fn error_suppressing_assignment_refinement(
        &mut self,
        local_ty: TypeId,
        value_ty: TypeId,
    ) -> Option<TypeId> {
        if !self.is_dynamic_assignment_source(local_ty)
            || self.is_dynamic_assignment_source(value_ty)
        {
            return None;
        }
        Some(self.union_type(vec![
            self.primitives().error,
            self.widen_mutable_literal_type(value_ty),
        ]))
    }
    fn push_annotation_subtype_constraint(
        &mut self,
        sub: TypeId,
        sup: TypeId,
        location: Option<Location>,
    ) {
        let diagnostic_location = location.map(DiagnosticLocation::from);
        let explicit_location = self.generic_count_mismatch_location(sub, sup, location);
        if explicit_location.is_some() {
            self.generated
                .constraints
                .push(Constraint::subtype(sub, sup, explicit_location));
        } else if diagnostic_location.is_some() {
            self.generated
                .constraints
                .push(Constraint::subtype_default_location(
                    sub,
                    sup,
                    diagnostic_location,
                ));
        } else {
            self.generated
                .constraints
                .push(Constraint::subtype(sub, sup, None));
        }
    }
    fn generic_count_mismatch_location(
        &self,
        sub: TypeId,
        sup: TypeId,
        location: Option<Location>,
    ) -> Option<DiagnosticLocation> {
        let TypeKind::Function(sup_fn) = self.arena.get(self.arena.follow(sup)) else {
            return None;
        };
        if sup_fn.generics.is_empty() && sup_fn.generic_packs.is_empty() {
            return None;
        }
        match self.arena.get(self.arena.follow(sub)) {
            TypeKind::Function(sub_fn)
                if sub_fn.generics.len() >= sup_fn.generics.len()
                    && sub_fn.generic_packs.len() >= sup_fn.generic_packs.len() =>
            {
                None
            }
            _ => location.map(DiagnosticLocation::from),
        }
    }
    pub(crate) fn assign_unannotated_local_type(
        &mut self,
        local_id: LocalId,
        local_ty: TypeId,
        value_ty: TypeId,
        refresh_captured_query_reads: bool,
    ) {
        let started_as_nil = self.nil_tracking.local_starts_as_nil(local_id);
        let stored_ty = self.widen_mutable_literal_type(value_ty);
        let assignable_local_ty = self.widen_mutable_literal_type(self.arena.follow(local_ty));
        if matches!(
            self.arena.get(self.arena.follow(local_ty)),
            TypeKind::Free(_)
        ) {
            self.bind_free_to(local_ty, stored_ty);
        } else if matches!(
            self.arena.get(self.arena.follow(stored_ty)),
            TypeKind::Table(_)
        ) {
            self.arena.replace(local_ty, TypeKind::Bound(stored_ty));
        } else if !self.is_dynamic(assignable_local_ty) && !self.is_dynamic(stored_ty) {
            let widened = self.union_type(vec![assignable_local_ty, stored_ty]);
            self.arena.replace(local_ty, TypeKind::Bound(widened));
        }
        self.merge_current_refinements(RefinementMap::from([(
            RefinementKey::Symbol(Symbol::Local(local_id)),
            stored_ty,
        )]));
        let declared_ty = self.widen_mutable_literal_type(self.arena.follow(local_ty));
        if refresh_captured_query_reads {
            self.refresh_captured_nil_query_reads(local_id, declared_ty);
        }
        if started_as_nil {
            self.record_started_as_nil_query_type(local_id, declared_ty);
        }
    }
    fn refresh_captured_nil_query_reads(&mut self, local_id: LocalId, assigned_ty: TypeId) {
        let Some(reads) = self
            .query_capture
            .captured_nil_reads
            .get(&local_id)
            .cloned()
        else {
            return;
        };
        for read in reads {
            let Some(read_ty) = self.captured_nil_query_read_type(assigned_ty, &read.path) else {
                continue;
            };
            if self.arena.is_nil(read_ty) {
                continue;
            }
            let query_ty = self.union_type(vec![
                self.primitives().nil,
                self.widen_mutable_literal_type(read_ty),
            ]);
            self.record_actual(read.location, read.syntax_id, query_ty);
        }
    }
    fn captured_nil_query_read_type(&self, base_ty: TypeId, path: &[String]) -> Option<TypeId> {
        let mut ty = base_ty;
        for component in path {
            ty = self.arena.direct_read_property(ty, component)?;
        }
        Some(ty)
    }
    fn record_started_as_nil_query_type(&mut self, local_id: LocalId, ty: TypeId) {
        if !self.nil_tracking.local_starts_as_nil(local_id) {
            return;
        }
        let mut types = vec![self.primitives().nil, self.widen_mutable_literal_type(ty)];
        if let Some(existing) = self.generated.query_local_types.get(&local_id).copied() {
            types.push(existing);
        }
        let query_ty = self.union_type(types);
        self.generated.query_local_types.insert(local_id, query_ty);
    }
    pub(crate) fn rebind_nil_initialized_local_assignment(
        &mut self,
        local_id: LocalId,
        local_ty: TypeId,
        scope: ScopeId,
        value: &Expr,
    ) -> bool {
        if self.nil_tracking.initialized_locals.contains(&local_id)
            && !self
                .refinements
                .nonfallthrough_loop_assignment_snapshots
                .is_empty()
        {
            let value_ty = self.expr_type(scope, value);
            let stored_ty = widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
            self.record_started_as_nil_query_type(local_id, stored_ty);
            self.refine_current_local(local_id, stored_ty);
            return true;
        }
        if self.nil_tracking.typeof_snapshot_locals.contains(&local_id)
            && matches!(
                self.arena.get(self.arena.follow(local_ty)),
                TypeKind::Free(_)
            )
        {
            let value_ty = self.expr_type(scope, value);
            let value_ty = widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
            self.snapshot_nonfallthrough_loop_assignment(local_ty);
            if self.loop_assignment_may_be_skipped() {
                self.bind_loop_assignment_as_optional(local_ty, value_ty);
            } else if self.is_dynamic(value_ty) {
                self.bind_free_to(local_ty, value_ty);
            } else {
                let stored_ty = self.union_type(vec![self.primitives().nil, value_ty]);
                self.bind_free_to(local_ty, stored_ty);
            }
            self.refine_current_local(local_id, value_ty);
            return true;
        }

        match self.arena.get(local_ty) {
            TypeKind::Bound(bound) if *bound == self.primitives().nil => {}
            _ => return false,
        }
        let value_ty = self.expr_type(scope, value);
        let value_ty = widened_table_literal_value_type(self.arena, value).unwrap_or(value_ty);
        self.snapshot_nonfallthrough_loop_assignment(local_ty);
        if self.loop_assignment_may_be_skipped() {
            self.bind_loop_assignment_as_optional(local_ty, value_ty);
            self.refine_current_local(local_id, value_ty);
        } else if self.nil_tracking.typeof_snapshot_locals.contains(&local_id)
            && !self.is_dynamic(value_ty)
        {
            let stored_ty = self.union_type(vec![self.primitives().nil, value_ty]);
            self.arena.replace(local_ty, TypeKind::Bound(stored_ty));
            self.refine_current_local(local_id, value_ty);
        } else {
            self.arena.replace(local_ty, TypeKind::Bound(value_ty));
        }
        true
    }
    pub(crate) fn should_bind_loop_assignment_as_optional(
        &self,
        local_id: LocalId,
        local_ty: TypeId,
    ) -> bool {
        self.loop_assignment_may_be_skipped()
            && !self.nil_tracking.initialized_locals.contains(&local_id)
            && matches!(
                self.arena.get(self.arena.follow(local_ty)),
                TypeKind::Free(_)
            )
    }
    pub(crate) fn bind_loop_assignment_as_optional(&mut self, local_ty: TypeId, value_ty: TypeId) {
        let value_ty = self.widen_mutable_literal_type(value_ty);
        let assigned_ty = if self.is_for_in_dynamic_recovery(value_ty) || !self.is_dynamic(value_ty)
        {
            self.union_type(vec![value_ty, self.primitives().nil])
        } else {
            value_ty
        };
        self.snapshot_nonfallthrough_loop_assignment(local_ty);
        self.bind_free_to(local_ty, assigned_ty);
    }
    pub(crate) fn should_widen_loop_assignment(&self, local: &LocalRef, local_ty: TypeId) -> bool {
        self.loop_depth > 0
            && local.luau_type.is_none()
            && matches!(self.arena.get(local_ty), TypeKind::Bound(_))
    }
    pub(crate) fn widen_loop_assignment(&mut self, local_ty: TypeId, value_ty: TypeId) {
        let TypeKind::Bound(current) = self.arena.get(local_ty).clone() else {
            return;
        };
        let current = self.widen_mutable_literal_type(current);
        let value_ty = self.widen_mutable_literal_type(value_ty);
        let widened = if self.is_dynamic(value_ty) {
            value_ty
        } else if self.loop_assignment_may_be_skipped() {
            self.union_type(vec![current, value_ty])
        } else {
            value_ty
        };
        self.snapshot_nonfallthrough_loop_assignment(local_ty);
        self.arena.replace(local_ty, TypeKind::Bound(widened));
    }
    pub(crate) fn widen_nil_initialized_loop_assignment(
        &mut self,
        local_ty: TypeId,
        value_ty: TypeId,
    ) {
        let TypeKind::Bound(current) = self.arena.get(local_ty).clone() else {
            return;
        };
        let current = self.widen_mutable_literal_type(current);
        let value_ty = self.widen_mutable_literal_type(value_ty);
        let widened = if self.is_dynamic(value_ty) {
            value_ty
        } else if self.loop_assignment_may_be_skipped() {
            self.union_type(vec![current, value_ty, self.primitives().nil])
        } else {
            value_ty
        };
        self.snapshot_nonfallthrough_loop_assignment(local_ty);
        self.arena.replace(local_ty, TypeKind::Bound(widened));
    }
    pub(crate) fn loop_assignment_may_be_skipped(&self) -> bool {
        self.loop_depth > self.repeat_guaranteed_body_depth
    }
    pub(crate) fn snapshot_nonfallthrough_loop_assignment(&mut self, local_ty: TypeId) {
        let kind = self.arena.get(local_ty).clone();
        self.snapshot_nonfallthrough_loop_assignment_with(local_ty, kind);
    }
    pub(crate) fn snapshot_nonfallthrough_loop_assignment_with(
        &mut self,
        local_ty: TypeId,
        kind: TypeKind,
    ) {
        let Some(snapshot) = self
            .refinements
            .nonfallthrough_loop_assignment_snapshots
            .last_mut()
        else {
            return;
        };
        snapshot.entry(local_ty).or_insert(kind);
    }
    pub(crate) fn restore_nonfallthrough_loop_assignments(&mut self) {
        let Some(snapshot) = self
            .refinements
            .nonfallthrough_loop_assignment_snapshots
            .pop()
        else {
            return;
        };
        self.restore_assignment_snapshot(snapshot);
    }
    pub(crate) fn restore_assignment_snapshot(&mut self, snapshot: BTreeMap<TypeId, TypeKind>) {
        for (ty, kind) in snapshot {
            let kind = match kind {
                TypeKind::Bound(inner) => TypeKind::Bound(self.widen_mutable_literal_type(inner)),
                kind => kind,
            };
            self.arena.replace(ty, kind);
        }
    }
    pub(crate) fn merge_branch_refinements(
        &mut self,
        branches: impl IntoIterator<Item = RefinementMap>,
    ) -> RefinementMap {
        let branches = branches.into_iter().collect::<Vec<_>>();
        let mut keys = BTreeMap::new();
        for branch in &branches {
            for key in branch.keys() {
                keys.insert(key.clone(), ());
            }
        }

        let mut merged = RefinementMap::new();
        for key in keys.into_keys() {
            let mut options = Vec::new();
            for branch in &branches {
                if let Some(ty) = branch
                    .get(&key)
                    .copied()
                    .or_else(|| self.refined_type(&key))
                    .or_else(|| self.base_refinement_type(&key))
                    && !options.contains(&ty)
                {
                    options.push(ty);
                }
            }
            if options.len() == 1 {
                merged.insert(key, options[0]);
            } else if !options.is_empty() {
                let ty = self.union_type(options);
                merged.insert(key, ty);
            }
        }
        merged
    }
    pub(crate) fn branch_assignment_refinements(
        entry: &RefinementMap,
        after: &RefinementMap,
    ) -> RefinementMap {
        after
            .iter()
            .filter_map(|(key, ty)| {
                if entry.get(key).copied() == Some(*ty) {
                    None
                } else {
                    Some((key.clone(), *ty))
                }
            })
            .collect()
    }
    pub(crate) fn record_query_refinement_types(&mut self, refinements: &RefinementMap) {
        for (key, ty) in refinements {
            if let RefinementKey::Symbol(Symbol::Local(local_id)) = key {
                let query_ty = self.widen_mutable_literal_type(*ty);
                self.generated.query_local_types.insert(*local_id, query_ty);
            }
        }
    }
    pub(crate) fn merged_refinement_keys<const N: usize>(
        maps: [&RefinementMap; N],
    ) -> BTreeMap<RefinementKey, ()> {
        let mut keys = BTreeMap::new();
        for map in maps {
            for key in map.keys() {
                keys.insert(key.clone(), ());
            }
        }
        keys
    }
    pub(crate) fn refinements_for_keys(
        refinements: &RefinementMap,
        keys: &BTreeMap<RefinementKey, ()>,
    ) -> RefinementMap {
        keys.keys()
            .filter_map(|key| refinements.get(key).copied().map(|ty| (key.clone(), ty)))
            .collect()
    }
    pub(crate) fn base_refinement_type(&self, key: &RefinementKey) -> Option<TypeId> {
        match key {
            RefinementKey::Symbol(Symbol::Local(local)) => {
                let def = self.input.dfg.local(*local)?;
                let ty = self.input.dfg.get(def).ty;
                if self
                    .input
                    .scopes
                    .lookup_local_id(*local)
                    .is_some_and(|binding| binding.kind == ValueBindingKind::FunctionParameter)
                    && matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Free(_))
                {
                    return None;
                }
                if self.nil_tracking.local_starts_as_nil(*local)
                    && matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Free(_))
                {
                    Some(self.primitives().nil)
                } else {
                    Some(ty)
                }
            }
            RefinementKey::Symbol(Symbol::Global(global)) => self
                .input
                .scopes
                .lookup_global(self.input.scopes.root(), global)
                .and_then(|binding| binding.ty)
                .or_else(|| self.generated.global_defs.get(global).copied()),
            RefinementKey::Symbol(Symbol::Empty) => None,
            RefinementKey::Property { .. } => self
                .input
                .dfg
                .current_key(key)
                .map(|def| self.input.dfg.get(def).ty),
        }
    }
    pub(crate) fn stat_exits(&self, stat: &Stat) -> bool {
        match stat {
            Stat::Return { .. } | Stat::Break { .. } | Stat::Continue { .. } => true,
            Stat::Block { body, .. } => body.iter().any(|stat| self.stat_exits(stat)),
            Stat::If {
                then_body,
                else_body,
                ..
            } => {
                self.stat_exits(then_body)
                    && else_body
                        .as_deref()
                        .is_some_and(|else_body| self.stat_exits(else_body))
            }
            Stat::Repeat { body, .. } => self.stat_always_exits(body),
            Stat::Expr { expr, .. } => self.expr_exits(expr),
            _ => false,
        }
    }
    pub(crate) fn stat_always_exits(&self, stat: &Stat) -> bool {
        match stat {
            Stat::Return { .. } | Stat::Break { .. } | Stat::Continue { .. } => true,
            Stat::Block { body, .. } => body.iter().any(|stat| self.stat_always_exits(stat)),
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                if self.expr_is_truthy_literal(condition) {
                    return self.stat_always_exits(then_body);
                }
                if self.expr_is_falsy_literal(condition) {
                    return else_body
                        .as_deref()
                        .is_some_and(|else_body| self.stat_always_exits(else_body));
                }
                self.stat_always_exits(then_body)
                    && else_body
                        .as_deref()
                        .is_some_and(|else_body| self.stat_always_exits(else_body))
            }
            Stat::While {
                condition, body, ..
            } => self.expr_is_truthy_literal(condition) && !Self::stat_may_break_current_loop(body),
            Stat::Repeat {
                condition, body, ..
            } => {
                !Self::stat_may_break_current_loop(body)
                    && (self.stat_always_exits(body) || self.expr_is_falsy_literal(condition))
            }
            Stat::Expr { expr, .. } => self.expr_exits(expr),
            _ => false,
        }
    }
    pub(crate) fn stat_may_break_current_loop(stat: &Stat) -> bool {
        match stat {
            Stat::Break { .. } => true,
            Stat::Block { body, .. } => body.iter().any(Self::stat_may_break_current_loop),
            Stat::If {
                then_body,
                else_body,
                ..
            } => {
                Self::stat_may_break_current_loop(then_body)
                    || else_body
                        .as_deref()
                        .is_some_and(Self::stat_may_break_current_loop)
            }
            Stat::While { .. } | Stat::Repeat { .. } | Stat::For { .. } | Stat::ForIn { .. } => {
                false
            }
            _ => false,
        }
    }
    pub(crate) fn stat_return(
        &mut self,
        scope: ScopeId,
        location: Option<Location>,
        list: &[Expr],
    ) {
        if let Some(return_seen) = self.function_frames.return_seen_stack.last_mut() {
            *return_seen = true;
        }
        let unannotated_return = self
            .function_frames
            .unannotated_return_stack
            .last()
            .copied()
            .unwrap_or(false);
        let contextual_return = self
            .function_frames
            .contextual_return_stack
            .last()
            .copied()
            .unwrap_or(false);
        let expected_returns = self.function_frames.return_stack.last().copied();
        let expected_pack = expected_returns.map(|pack| self.arena.normalize_pack(pack));
        let expected_types = expected_pack
            .as_ref()
            .map(|pack| pack.types.clone())
            .unwrap_or_default();
        // A variadic return tail (`...T`) is the expected type for every
        // returned value past the fixed prefix, so a returned literal is
        // checked against `T` and widens to it (`infer_return_value_type`:
        // `function h(): ...{string|number} return {4}, ... end` — `{4}` must be
        // checked against `{string|number}`, not left as the invariant `{number}`).
        let expected_variadic_tail = expected_pack.as_ref().and_then(|pack| match pack.tail {
            Some(TypePackTail::Variadic(ty)) => Some(ty),
            _ => None,
        });
        let mut actual_returns = Vec::new();
        let mut actual_return_was_dynamic = Vec::new();
        let mut actual_return_table_literals = Vec::new();
        let mut actual_return_preserve = Vec::new();
        let mut actual_return_pack = None;
        if let [expr] = list
            && !expr_is_table_freeze_call(expr)
            && let Some(return_count) = self.call_fixed_return_count(scope, expr)
            && let Some(return_values) = self.call_return_values(scope, expr, return_count, &[])
        {
            for ty in return_values.into_iter().flatten() {
                actual_return_was_dynamic.push(self.is_dynamic(ty));
                actual_returns.push(ty);
                actual_return_table_literals.push(false);
                actual_return_preserve.push(false);
            }
        } else if let [expr] = list
            && !expr_is_table_freeze_call(expr)
            && let Some(return_pack) = self.preserved_call_return_pack(scope, expr)
        {
            actual_return_pack = Some(return_pack);
            for ty in self.arena.normalize_pack(return_pack).types {
                actual_return_was_dynamic.push(self.is_dynamic(ty));
                actual_returns.push(ty);
                actual_return_table_literals.push(false);
                actual_return_preserve.push(false);
            }
        } else {
            for (index, expr) in list.iter().enumerate() {
                let expected = expected_types
                    .get(index)
                    .copied()
                    .or(expected_variadic_tail);
                let actual = self.expr_type_with_expected_aggregation(scope, expr, expected, true);
                actual_return_was_dynamic.push(self.is_dynamic(actual));
                actual_return_table_literals.push(expr_is_table_literal(expr));
                // A `:: T` assertion's type is authoritative — preserve it.
                actual_return_preserve
                    .push(matches!(ungroup_expr(expr), Expr::TypeAssertion { .. }));
                actual_returns.push(
                    expected
                        .filter(|_| self.is_permissive_return_actual(expr, actual))
                        .unwrap_or(actual),
                );
            }
        }
        if unannotated_return
            && list.iter().any(|expr| {
                expr_contains_any_syntax(expr, &self.operator.never_arithmetic_exprs)
                    || expr_is_logical_binary_containing_any_syntax(
                        expr,
                        &self.operator.recursive_arithmetic_exprs,
                    )
            })
        {
            let recommended_return = match actual_returns.as_slice() {
                [only] => Some(self.arena.summary(*only)),
                [] => Some("()".to_owned()),
                _ => Some(format!(
                    "({})",
                    actual_returns
                        .iter()
                        .map(|ty| self.arena.summary(*ty))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            };
            let diagnostic = Diagnostic::error(
                DiagnosticCategory::Generic,
                DiagnosticLocation::from_opt(location),
            )
            .with_typed(
                crate::diagnostics::Payload::ExplicitFunctionAnnotationRecommended {
                    recommended_return,
                    recommended_args: None,
                },
            );
            self.generated.diagnostics.push(diagnostic);
        }
        self.seed_unannotated_return_inference(
            unannotated_return,
            contextual_return,
            actual_return_pack,
            &actual_returns,
            &actual_return_preserve,
        );
        if let Some(inferred_returns) = self.function_frames.inferred_return_stack.last_mut() {
            inferred_returns.push(InferredReturnPath {
                fixed: actual_returns
                    .iter()
                    .copied()
                    .zip(actual_return_table_literals)
                    .zip(actual_return_preserve)
                    .map(|((ty, table_literal), preserve)| InferredReturnType {
                        ty,
                        table_literal,
                        preserve,
                    })
                    .collect(),
                pack: actual_return_pack,
            });
        }
        if self.input.mode == AnalysisMode::Nonstrict && unannotated_return && !contextual_return {
            return;
        }
        let actual_return_has_open_tail = actual_return_pack
            .map(|pack| self.arena.normalize_pack(pack).tail.is_some())
            .unwrap_or(false);
        let return_arity_mismatch = self.input.mode == AnalysisMode::Strict
            && expected_pack.as_ref().is_some_and(|pack| {
                pack.tail.is_none()
                    && !actual_return_has_open_tail
                    && actual_returns.len() != pack.types.len()
            });
        if return_arity_mismatch {
            let expected = expected_pack.as_ref().map_or(0, |pack| pack.types.len());
            let actual = actual_returns.len();
            let diagnostic = Diagnostic::error(
                DiagnosticCategory::TypePack,
                location.map_or_else(DiagnosticLocation::missing, DiagnosticLocation::from),
            )
            .with_typed(crate::diagnostics::Payload::ReturnArityMismatch { expected, actual });
            self.generated.diagnostics.push(diagnostic);
        } else if let Some(expected_returns) = expected_returns {
            if unannotated_return
                && contextual_return
                && !Self::contextual_return_pack_needs_inference(self.arena, expected_returns)
            {
                return;
            }
            let actual_returns = if unannotated_return && actual_return_pack.is_none() {
                actual_returns
                    .into_iter()
                    .map(|ty| self.widen_inferred_return_type(ty, false))
                    .collect()
            } else {
                actual_returns
            };
            let nonstrict_fixed_expected_len =
                (self.input.mode == AnalysisMode::Nonstrict).then(|| {
                    expected_pack
                        .as_ref()
                        .and_then(|pack| pack.tail.is_none().then_some(pack.types.len()))
                });
            let nonstrict_fixed_expected_len = nonstrict_fixed_expected_len.flatten();
            let (actual_returns, expected_returns) = if actual_return_pack.is_none()
                && let Some(expected_len) = nonstrict_fixed_expected_len
            {
                let compare_len = actual_returns.len().min(expected_len);
                let actual_returns =
                    self.pack(actual_returns.into_iter().take(compare_len).collect());
                let expected_returns =
                    self.pack(expected_types.into_iter().take(compare_len).collect());
                (actual_returns, expected_returns)
            } else if actual_return_pack.is_none()
                && list.len() == 1
                && actual_return_was_dynamic.first().copied().unwrap_or(false)
                && expected_pack
                    .as_ref()
                    .is_some_and(|pack| pack.tail.is_some())
            {
                let tail = self.arena.alloc_pack(TypePackKind::Variadic {
                    ty: self.primitives().any,
                });
                (
                    self.pack_with_tail(expected_types, Some(tail)),
                    expected_returns,
                )
            } else {
                (
                    actual_return_pack.unwrap_or_else(|| self.pack(actual_returns)),
                    expected_returns,
                )
            };
            self.generated
                .constraints
                .push(Constraint::pack_subtype_default_location(
                    actual_returns,
                    expected_returns,
                    location.map(DiagnosticLocation::from),
                ));
        }
    }

    fn contextual_return_pack_needs_inference(arena: &Arena, expected_returns: TypePackId) -> bool {
        let normalized = arena.normalize_pack(expected_returns);
        normalized.types.is_empty()
            && matches!(
                normalized.tail,
                Some(TypePackTail::Free { .. } | TypePackTail::Generic(_))
            )
    }

    fn is_permissive_return_actual(&self, expr: &Expr, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Any => matches!(ungroup_expr(expr), Expr::TypeAssertion { .. }),
            TypeKind::Error | TypeKind::Blocked(_) => true,
            _ => false,
        }
    }
    fn seed_unannotated_return_inference(
        &mut self,
        unannotated_return: bool,
        contextual_return: bool,
        actual_return_pack: Option<TypePackId>,
        actual_returns: &[TypeId],
        actual_return_preserve: &[bool],
    ) {
        // Keep the function surface tied to the first inferred return shape,
        // while the ordinary return constraints continue to report any later
        // inconsistent paths.
        if !unannotated_return || contextual_return {
            return;
        }
        if self
            .function_frames
            .inferred_return_seed_stack
            .last()
            .is_some_and(Option::is_some)
        {
            return;
        }
        let seed = actual_return_pack.unwrap_or_else(|| {
            let types = actual_returns
                .iter()
                .copied()
                .zip(actual_return_preserve.iter().copied())
                .map(|(ty, preserve)| {
                    if preserve {
                        ty
                    } else {
                        self.widen_inferred_return_type(ty, false)
                    }
                })
                .collect();
            self.pack(types)
        });
        if let Some(slot) = self.function_frames.inferred_return_seed_stack.last_mut() {
            *slot = Some(seed);
        }
    }
    pub(crate) fn stat_for_in(
        &mut self,
        scope: ScopeId,
        vars: &[Local],
        values: &[Expr],
        body: &Stat,
    ) {
        let value_types = values
            .iter()
            .map(|value| self.expr_type_discarding_call_results(scope, value))
            .collect::<Vec<_>>();
        let zero_value_iterator_reported =
            self.report_zero_value_for_in_iterator(scope, values, &value_types);
        if let Some(first_value) = value_types.first().copied()
            && !zero_value_iterator_reported
            && self.is_known_non_iterable_for_in_value(first_value)
        {
            let empty = self.pack(Vec::new());
            self.generated.constraints.push(Constraint::call(
                first_value,
                empty,
                self.input.mode == AnalysisMode::Nonstrict,
                Vec::new(),
                None,
                values
                    .first()
                    .and_then(Expr::location)
                    .map(DiagnosticLocation::from),
                false,
            ));
        }
        self.constrain_for_in_iterator_arguments(values, &value_types);
        let body_scope = self.enter_child(scope);
        if let Some(loop_values) = self.for_in_loop_value_types(
            values,
            &value_types,
            vars.len(),
            zero_value_iterator_reported,
        ) {
            for (index, var) in vars.iter().enumerate() {
                let var_ty = self.local_type(var);
                let loop_value = loop_values
                    .get(index)
                    .copied()
                    .unwrap_or_else(|| self.primitives().nil);
                if let Some(annotation) = &var.luau_type {
                    let annotation_ty = self.lower_type(scope, annotation);
                    self.expect_type(var.location, var_ty, annotation_ty);
                    self.bind_free_to(var_ty, annotation_ty);
                    if !self.is_for_in_dynamic_recovery(loop_value) {
                        self.generated
                            .constraints
                            .push(Constraint::subtype_default_location(
                                loop_value,
                                annotation_ty,
                                var.location.map(DiagnosticLocation::from),
                            ));
                    }
                    self.generated
                        .constraints
                        .push(Constraint::unify(var_ty, annotation_ty));
                } else {
                    if self.input.mode == AnalysisMode::Nonstrict {
                        let any = self.primitives().any;
                        self.bind_free_to(var_ty, any);
                        self.generated
                            .constraints
                            .push(Constraint::unify(var_ty, any));
                        continue;
                    }
                    self.bind_free_to(var_ty, loop_value);
                    if !self.is_for_in_dynamic_recovery(loop_value) {
                        self.generated
                            .constraints
                            .push(Constraint::unify_default_location(
                                var_ty,
                                loop_value,
                                var.location.map(DiagnosticLocation::from),
                            ));
                    }
                }
            }
        } else {
            for var in vars {
                if let Some(annotation) = &var.luau_type {
                    let var_ty = self.local_type(var);
                    let annotation_ty = self.lower_type(scope, annotation);
                    self.expect_type(var.location, var_ty, annotation_ty);
                    self.bind_free_to(var_ty, annotation_ty);
                    self.generated
                        .constraints
                        .push(Constraint::unify(var_ty, annotation_ty));
                }
            }
        }
        self.visit_loop_body(body_scope, body, false);
    }
    pub(crate) fn stat_function(&mut self, scope: ScopeId, name: &Expr, func: &Expr) {
        if let Expr::Global {
            location,
            name: global_name,
            ..
        } = name
        {
            let placeholder = self
                .predeclared_global_function_placeholder(global_name.as_str())
                .unwrap_or_else(|| self.fresh_global_function_placeholder());
            self.generated
                .global_defs
                .insert(global_name.as_str().to_owned(), placeholder);
            let func_ty = self.with_next_global_function(global_name.as_str().to_owned(), |this| {
                this.expr_type(scope, func)
            });
            let stored_ty = if function_signature_has_callback_free_correlation(self.arena, func_ty)
            {
                generalize_function_signature_frees(self.arena, func_ty)
            } else {
                func_ty
            };
            drop(Unifier::new(self.arena).unify(placeholder, func_ty));
            self.generated
                .global_defs
                .insert(global_name.as_str().to_owned(), stored_ty);
            let name_ty = self.dfg_type_for_expr(name);
            self.bind_actual(*location, name.syntax_id(), name_ty, stored_ty);
        } else if let Expr::Local { local, .. } = name {
            self.lvalue_type(scope, name);
            let name_ty = self
                .input
                .dfg
                .local(local.id)
                .map(|def| self.input.dfg.get(def).ty)
                .unwrap_or_else(|| self.recovery_type_at(local.location, "missing local def"));
            let recursive_prototype = self.provisional_no_arg_function_type(func);
            if let Some(prototype) = recursive_prototype {
                self.refinements.locals.push(RefinementMap::from([(
                    RefinementKey::Symbol(Symbol::Local(local.id)),
                    prototype,
                )]));
            }
            let func_ty = self.expr_type(scope, func);
            if recursive_prototype.is_some() {
                self.refinements.locals.pop();
            }
            if local.luau_type.is_none()
                && !matches!(
                    self.arena.get(self.arena.follow(name_ty)),
                    TypeKind::Free(_)
                )
            {
                let current = self.arena.follow(self.widen_mutable_literal_type(name_ty));
                let merged = self.union_type(vec![current, func_ty, self.primitives().nil]);
                self.arena.replace(name_ty, TypeKind::Bound(merged));
            } else {
                drop(Unifier::new(self.arena).unify(name_ty, func_ty));
                self.generated
                    .constraints
                    .push(Constraint::unify_default_location(
                        name_ty,
                        func_ty,
                        local.location.map(DiagnosticLocation::from),
                    ));
            }
            if local.luau_type.is_none() {
                self.record_started_as_nil_query_type(local.id, func_ty);
            }
        } else if is_plain_index_function_name(name) {
            if let Some((expected, expected_from_intersection)) =
                self.plain_index_function_expected_type(scope, name)
            {
                self.assign_function_to_declared_index(
                    scope,
                    name,
                    func,
                    expected,
                    expected_from_intersection,
                );
            } else {
                let prototype = self.table_function_property_prototype(scope, name, func);
                let func_ty = self.expr_type(scope, func);
                if let Some(prototype) = prototype {
                    drop(Unifier::new(self.arena).unify(prototype, func_ty));
                }
                self.materialize_unsealed_property_writes_in_type(func_ty);
                let stored_ty = if plain_index_function_base(name)
                    .is_some_and(|base| !function_references_base(func, base))
                {
                    generalize_function_frees(self.arena, func_ty)
                } else {
                    func_ty
                };
                self.assign_known_type_to_lvalue(scope, name, stored_ty);
            }
        } else if self_index_function_name(name) {
            if let Some((expected, expected_from_intersection)) =
                self.self_index_function_expected_type(scope, name)
            {
                let actual = if self.expected_function_needs_ascription(expected) {
                    self.expr_type_with_function_parameter_context(scope, func, expected)
                } else {
                    self.expr_type_with_function_parameter_context_without_ascription(
                        scope, func, expected,
                    )
                };
                if expected_from_intersection
                    && function_body_has_return(func)
                    && !self.is_dynamic(actual)
                {
                    self.generated
                        .constraints
                        .push(Constraint::expected_subtype(
                            actual,
                            expected,
                            func.location().map(DiagnosticLocation::from),
                            true,
                        ));
                }
                self.assign_known_type_to_lvalue(scope, name, expected);
            } else {
                let func_ty = if let Some(expected) =
                    self.self_index_function_context_type(scope, name, func)
                {
                    self.expr_type_with_function_parameter_context(scope, func, expected)
                } else {
                    self.expr_type(scope, func)
                };
                self.assign_known_type_to_lvalue(scope, name, func_ty);
            }
        } else {
            let func_ty = self.expr_type(scope, func);
            let name_ty = self.lvalue_type(scope, name);
            self.generated
                .constraints
                .push(Constraint::unify(name_ty, func_ty));
        }
    }

    /// Checks a function value written to a property with a declared function
    /// type and stores the declared type back, keeping the property's surface
    /// stable across writes. Shared by `function t.f(x)` statements and
    /// `t.f = function(x)` assignments so both forms report the same
    /// whole-function mismatch (e.g. a contextual return-type error) instead
    /// of the assignment form silently ascribing the declared type.
    fn assign_function_to_declared_index(
        &mut self,
        scope: ScopeId,
        name: &Expr,
        func: &Expr,
        expected: TypeId,
        expected_from_intersection: bool,
    ) {
        let actual = if self.expected_function_needs_ascription(expected) {
            self.expr_type_with_function_parameter_context(scope, func, expected)
        } else {
            self.expr_type_with_function_parameter_context_without_ascription(scope, func, expected)
        };
        if (expected_from_intersection || !self.expected_accepts_without_subtype(actual, expected))
            && function_body_has_return(func)
            && !self.is_dynamic(actual)
            && !self.is_error_type(expected)
        {
            self.generated
                .constraints
                .push(Constraint::expected_subtype(
                    actual,
                    expected,
                    func.location().map(DiagnosticLocation::from),
                    true,
                ));
        }
        self.assign_known_type_to_lvalue(scope, name, expected);
    }

    fn table_function_property_prototype(
        &mut self,
        scope: ScopeId,
        name: &Expr,
        func: &Expr,
    ) -> Option<TypeId> {
        let Expr::IndexName {
            expr: base, index, ..
        } = name
        else {
            return None;
        };
        if !function_body_reads_property(func, base, index.as_str()) {
            return None;
        }
        let prototype = self.function_header_prototype_type(scope, func, false)?;
        let base_ty = self.expr_type(scope, base);
        self.insert_table_property_prototype(base_ty, index.as_str(), prototype)
            .then_some(prototype)
    }

    fn plain_index_function_expected_type(
        &mut self,
        scope: ScopeId,
        name: &Expr,
    ) -> Option<(TypeId, bool)> {
        match name {
            Expr::IndexName {
                expr: base,
                index,
                op,
                ..
            } if *op == "." => {
                let base_ty = self.expr_type(scope, base);
                let (property_ty, from_intersection) =
                    self.self_index_function_property_type(base_ty, index.as_str())?;
                self.expected_function_for_literal(property_ty)
                    .map(|expected| (expected, from_intersection))
            }
            Expr::Group { expr, .. } => self.plain_index_function_expected_type(scope, expr),
            _ => None,
        }
    }

    fn self_index_function_expected_type(
        &mut self,
        scope: ScopeId,
        name: &Expr,
    ) -> Option<(TypeId, bool)> {
        match name {
            Expr::IndexName {
                expr: base,
                index,
                op,
                ..
            } if *op == ":" => {
                let base_ty = self.expr_type(scope, base);
                let (property_ty, from_intersection) =
                    self.self_index_function_property_type(base_ty, index.as_str())?;
                self.expected_function_for_literal(property_ty)
                    .map(|expected| (expected, from_intersection))
            }
            Expr::Group { expr, .. } => self.self_index_function_expected_type(scope, expr),
            _ => None,
        }
    }

    fn self_index_function_property_type(
        &mut self,
        ty: TypeId,
        property: &str,
    ) -> Option<(TypeId, bool)> {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Intersection(types) => {
                let properties = types
                    .into_iter()
                    .filter_map(|ty| {
                        self.self_index_function_property_type(ty, property)
                            .map(|(ty, _)| ty)
                    })
                    .collect::<Vec<_>>();
                if properties
                    .iter()
                    .any(|property| self.arena.follow(*property) == self.primitives().any)
                {
                    return Some((self.primitives().any, true));
                }
                (!properties.is_empty()).then(|| (self.intersection_type(properties), true))
            }
            _ => self.property_type(ty, property).map(|ty| (ty, false)),
        }
    }

    fn expr_type_with_function_parameter_context(
        &mut self,
        scope: ScopeId,
        func: &Expr,
        expected: TypeId,
    ) -> TypeId {
        self.expr_type_with_function_parameter_context_inner(scope, func, expected, true)
    }

    fn expr_type_with_function_parameter_context_without_ascription(
        &mut self,
        scope: ScopeId,
        func: &Expr,
        expected: TypeId,
    ) -> TypeId {
        self.expr_type_with_function_parameter_context_inner(scope, func, expected, false)
    }

    fn expr_type_with_function_parameter_context_inner(
        &mut self,
        scope: ScopeId,
        func: &Expr,
        expected: TypeId,
        allow_ascription: bool,
    ) -> TypeId {
        let previous = self.expected_by_syntax.insert(func.syntax_id(), expected);
        let disable_ascription = !allow_ascription
            && self
                .non_ascribing_contextual_functions
                .insert(func.syntax_id());
        let ty = self.expr_type(scope, func);
        if disable_ascription {
            self.non_ascribing_contextual_functions
                .remove(&func.syntax_id());
        }
        if let Some(previous) = previous {
            self.expected_by_syntax.insert(func.syntax_id(), previous);
        } else {
            self.expected_by_syntax.remove(&func.syntax_id());
        }
        ty
    }

    fn expected_function_needs_ascription(&self, expected: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(expected)),
            TypeKind::Function(function)
                if !function.generics.is_empty() || !function.generic_packs.is_empty()
        )
    }

    fn function_header_prototype_type(
        &mut self,
        scope: ScopeId,
        func: &Expr,
        allow_free_arguments: bool,
    ) -> Option<TypeId> {
        self.function_header_prototype_type_with_self(scope, func, allow_free_arguments, None)
    }

    fn function_header_prototype_type_with_self(
        &mut self,
        scope: ScopeId,
        func: &Expr,
        allow_free_arguments: bool,
        self_type: Option<TypeId>,
    ) -> Option<TypeId> {
        let Expr::Function {
            generics,
            generic_packs,
            args,
            self_arg,
            vararg,
            vararg_annotation,
            return_annotation,
            ..
        } = func
        else {
            return None;
        };
        if !generics.is_empty() || !generic_packs.is_empty() {
            return None;
        }
        if !allow_free_arguments
            && (self_arg
                .iter()
                .chain(args.iter())
                .any(|arg| arg.luau_type.is_none())
                || (*vararg && vararg_annotation.is_none()))
        {
            return None;
        }

        let mut argument_names = Vec::new();
        let mut arguments = Vec::new();
        if let Some(self_arg) = self_arg {
            argument_names.push(Some(self_arg.name.as_str().to_owned()));
            arguments.push(self_type.unwrap_or_else(|| self.local_prototype_type(scope, self_arg)));
        }
        for arg in args {
            argument_names.push(Some(arg.name.as_str().to_owned()));
            arguments.push(self.local_prototype_type(scope, arg));
        }
        let tail = if *vararg {
            Some(self.with_generic_alias_type_arguments(|this| {
                this.lower_vararg_type_pack_option(scope, vararg_annotation.as_deref())
            }))
        } else {
            None
        };
        let arguments = self.arena.alloc_pack(TypePackKind::List {
            types: arguments,
            tail,
        });
        let returns = if let Some(return_annotation) = return_annotation {
            self.with_function_signature_lowering(|this| {
                this.lower_type_pack(scope, return_annotation)
            })
        } else {
            self.arena.alloc_pack(TypePackKind::Free {
                level: TypeLevel(0),
                name: Some("table-function-return".to_owned()),
            })
        };
        let mut function = FunctionType::new(arguments, returns);
        function.argument_names = argument_names;
        function.has_self = self_arg.is_some();
        function.is_checked = true;
        Some(self.arena.alloc(TypeKind::Function(function)))
    }

    fn self_index_function_context_type(
        &mut self,
        scope: ScopeId,
        name: &Expr,
        func: &Expr,
    ) -> Option<TypeId> {
        let base = self_index_function_base(name)?;
        if function_signature_reads_base(func, base) {
            return None;
        }
        let self_type = self.expr_type(scope, base);
        if !self_method_call_properties(func)
            .iter()
            .any(|property| self.property_type_is_function(self_type, property))
        {
            return None;
        }
        self.function_header_prototype_type_with_self(scope, func, true, Some(self_type))
    }

    fn property_type_is_function(&mut self, ty: TypeId, property: &str) -> bool {
        self.property_type(ty, property).is_some_and(|property_ty| {
            matches!(
                self.arena.get(self.arena.follow(property_ty)),
                TypeKind::Function(_)
            )
        })
    }

    fn local_prototype_type(&mut self, scope: ScopeId, local: &Local) -> TypeId {
        local
            .luau_type
            .as_ref()
            .map(|annotation| {
                self.with_generic_alias_type_arguments(|this| this.lower_type(scope, annotation))
            })
            .unwrap_or_else(|| self.local_type(local))
    }

    fn insert_table_property_prototype(
        &mut self,
        table: TypeId,
        name: &str,
        prototype: TypeId,
    ) -> bool {
        let table = self.arena.follow(table);
        let TypeKind::Table(mut table_type) = self.arena.get(table).clone() else {
            return false;
        };
        if !matches!(table_type.state, TableState::Free | TableState::Unsealed)
            || table_type.properties.contains_key(name)
        {
            return false;
        }
        table_type
            .properties
            .insert(name.to_owned(), TableProperty::new(prototype));
        self.arena.replace(table, TypeKind::Table(table_type));
        true
    }

    fn predeclare_table_function_property_prototypes(&mut self, scope: ScopeId, body: &[Stat]) {
        for stat in body {
            let Stat::Function { name, func, .. } = stat else {
                continue;
            };
            let Expr::IndexName {
                expr: base, index, ..
            } = name.as_ref()
            else {
                continue;
            };
            if !is_plain_index_function_name(name) {
                continue;
            }
            if index.as_str().starts_with("__") {
                continue;
            }
            if !function_has_explicit_return_annotation(func) {
                continue;
            }
            if function_signature_reads_base(func, base) {
                continue;
            }
            let Some(prototype) = self.function_header_prototype_type(scope, func, false) else {
                continue;
            };
            let base_ty = self.expr_type(scope, base);
            let _ = self.insert_table_property_prototype(base_ty, index.as_str(), prototype);
        }
        self.predeclare_forward_referenced_method_prototypes(scope, body);
    }

    /// Predeclares prototypes for table methods that an *earlier* method in the
    /// same block forward-references (`function T:foo()` whose body calls
    /// `T:bar()` before `bar` is defined). Unlike the annotated dot-function
    /// pass above, this admits colon methods and unannotated parameters by
    /// building a prototype over the parameters' own type variables, which the
    /// later definition then unifies with through `write_property`. It is gated
    /// strictly to forward-referenced names so prototypes are never introduced
    /// for ordinary methods, which would otherwise perturb their inference.
    fn predeclare_forward_referenced_method_prototypes(&mut self, scope: ScopeId, body: &[Stat]) {
        let methods: Vec<(usize, &Expr, &Expr, &str)> = body
            .iter()
            .enumerate()
            .filter_map(|(index, stat)| {
                let Stat::Function { name, func, .. } = stat else {
                    return None;
                };
                let Expr::IndexName {
                    index: property, ..
                } = name.as_ref()
                else {
                    return None;
                };
                if property.as_str().starts_with("__") {
                    return None;
                }
                Some((index, name.as_ref(), func.as_ref(), property.as_str()))
            })
            .collect();

        for &(position, name, func, property) in &methods {
            let Expr::IndexName { expr: base, .. } = name else {
                continue;
            };
            let forward_referenced = methods.iter().any(|&(earlier, _, earlier_func, _)| {
                earlier < position
                    && (function_body_reads_property(earlier_func, base, property)
                        || function_body_reads_setmetatable_local_property(
                            earlier_func,
                            base,
                            property,
                        ))
            });
            if !forward_referenced {
                continue;
            }
            if function_signature_reads_base(func, base) {
                continue;
            }
            let Some(prototype) = self.function_header_prototype_type(scope, func, true) else {
                continue;
            };
            let base_ty = self.expr_type(scope, base);
            let _ = self.insert_table_property_prototype(base_ty, property, prototype);
        }
    }

    fn predeclare_block_function_prototypes(&mut self, scope: ScopeId, body: &[Stat]) {
        let mut written_globals = BTreeSet::new();
        for stat in body {
            self.record_preceding_global_writes(stat, &mut written_globals);
            let Stat::Function { name, .. } = stat else {
                continue;
            };
            let Expr::Global {
                name: global_name, ..
            } = name.as_ref()
            else {
                continue;
            };
            if written_globals.contains(global_name.as_str()) {
                continue;
            }
            if self
                .generated
                .global_defs
                .get(global_name.as_str())
                .is_some_and(|ty| !self.global_def_is_forward_placeholder(*ty))
            {
                continue;
            }
            if self
                .input
                .scopes
                .lookup_global(scope, global_name.as_str())
                .and_then(|binding| binding.ty)
                .is_some()
            {
                continue;
            }
            let placeholder = self.fresh_global_function_placeholder();
            self.generated
                .global_defs
                .insert(global_name.as_str().to_owned(), placeholder);
        }
    }

    fn record_preceding_global_writes(&self, stat: &Stat, written_globals: &mut BTreeSet<String>) {
        match stat {
            Stat::Assign { vars, .. } => {
                for var in vars {
                    if let Expr::Global { name, .. } = var {
                        written_globals.insert(name.as_str().to_owned());
                    }
                }
            }
            Stat::CompoundAssign { var, .. } => {
                if let Expr::Global { name, .. } = var.as_ref() {
                    written_globals.insert(name.as_str().to_owned());
                }
            }
            Stat::Block { body, .. } => {
                for stat in body {
                    self.record_preceding_global_writes(stat, written_globals);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::For { body, .. }
            | Stat::ForIn { body, .. } => {
                self.record_preceding_global_writes(body, written_globals);
            }
            Stat::If {
                then_body,
                else_body,
                ..
            } => {
                self.record_preceding_global_writes(then_body, written_globals);
                if let Some(else_body) = else_body {
                    self.record_preceding_global_writes(else_body, written_globals);
                }
            }
            Stat::Error { statements, .. } => {
                for stat in statements {
                    self.record_preceding_global_writes(stat, written_globals);
                }
            }
            _ => {}
        }
    }

    fn predeclared_global_function_placeholder(&self, name: &str) -> Option<TypeId> {
        let placeholder = *self.generated.global_defs.get(name)?;
        self.global_def_is_forward_placeholder(placeholder)
            .then_some(placeholder)
    }

    fn global_def_is_forward_placeholder(&self, ty: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(ty)),
            TypeKind::Free(crate::types::TypeVariable { .. })
        )
    }

    fn fresh_global_function_placeholder(&mut self) -> TypeId {
        self.arena.alloc(TypeKind::Free(crate::types::TypeVariable {
            level: TypeLevel(0),
            name: None,
            lower_bound: None,
            upper_bound: None,
        }))
    }
    pub(crate) fn provisional_no_arg_function_type(&mut self, func: &Expr) -> Option<TypeId> {
        let Expr::Function {
            args,
            self_arg,
            vararg,
            return_annotation,
            ..
        } = func
        else {
            return None;
        };
        if !args.is_empty() || self_arg.is_some() || *vararg || return_annotation.is_some() {
            return None;
        }
        let empty = self.arena.empty_pack();
        let mut function = crate::types::FunctionType::new(empty, empty);
        function.is_checked = true;
        Some(self.arena.alloc(TypeKind::Function(function)))
    }
}

fn expr_reads_global(expr: &Expr, global: &str) -> bool {
    let mut visitor = GlobalReadVisitor {
        global,
        found: false,
    };
    walk_expr(expr, &mut visitor);
    visitor.found
}

pub fn generate_expression_constraints_with_require_returns(
    module: &Stat,
    scopes: &ScopeTree,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    mode: AnalysisMode,
    config: GenerationConfig,
    require_return_types: &BTreeMap<SyntaxId, Vec<TypeId>>,
) -> GeneratedConstraints {
    let mut generator =
        ExpressionConstraintGenerator::new(scopes, dfg, arena, mode, config, require_return_types);
    generator.visit_stat(scopes.root(), module);
    generator.assert_frame_stacks_empty();
    generator.generated
}
