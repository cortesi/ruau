//! Query-only type surfaces built from solved module state.

mod recover;
#[cfg(any())]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

pub use recover::recover_nocheck_query_local_types;
use ruau_analysis::resolve::AnalysisMode;
use ruau_ast::{
    syntax::{Expr, LocalId, Stat},
    visit::{NodePath, Visitor, WalkControl, walk_stat},
};

use crate::{
    ast_util::ungroup_expr,
    dfg::DataFlowGraph,
    queries::Queries,
    types::{
        Arena, GenericType, GenericTypePack, PrimitiveType, SingletonType, TypeId, TypeKind,
        TypeLevel, TypePackId, TypePackKind,
    },
};

pub fn generalize_query_types_post_solve(
    root: &Stat,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    mode: AnalysisMode,
    queries: &Queries,
    global_defs: &mut BTreeMap<String, TypeId>,
    query_local_types: &mut BTreeMap<LocalId, TypeId>,
) {
    if mode == AnalysisMode::NoCheck {
        return;
    }
    for ty in global_defs.values_mut() {
        if matches!(arena.get(arena.follow(*ty)), TypeKind::Function(_)) {
            *ty = if mode == AnalysisMode::Nonstrict {
                crate::generalize::generalize_function_frees_to_unknown(arena, *ty)
            } else {
                crate::generalize::generalize_function_frees(arena, *ty)
            };
            *ty = crate::generalize::resolve_function_free_bounds_for_query(arena, *ty);
        }
    }
    generalize_local_function_query_types(root, dfg, arena, mode, query_local_types);
    widen_unannotated_singleton_query_types_in_stat(root, dfg, queries, arena, query_local_types);
    specialize_unannotated_function_query_arguments(root, arena, global_defs, query_local_types);
}

fn walk_query_stat_tree(stat: &Stat, enter_function_bodies: bool, visit: &mut impl FnMut(&Stat)) {
    visit(stat);
    match stat {
        Stat::Block { body, .. } => {
            for stat in body {
                walk_query_stat_tree(stat, enter_function_bodies, visit);
            }
        }
        Stat::If {
            then_body,
            else_body,
            ..
        } => {
            walk_query_stat_tree(then_body, enter_function_bodies, visit);
            if let Some(else_body) = else_body {
                walk_query_stat_tree(else_body, enter_function_bodies, visit);
            }
        }
        Stat::While { body, .. }
        | Stat::Repeat { body, .. }
        | Stat::For { body, .. }
        | Stat::ForIn { body, .. } => {
            walk_query_stat_tree(body, enter_function_bodies, visit);
        }
        Stat::Error { statements, .. }
        | Stat::Class {
            members: statements,
            ..
        } => {
            for stat in statements {
                walk_query_stat_tree(stat, enter_function_bodies, visit);
            }
        }
        Stat::Function { func, .. } | Stat::LocalFunction { func, .. } if enter_function_bodies => {
            if let Expr::Function { body, .. } = func.as_ref() {
                walk_query_stat_tree(body, enter_function_bodies, visit);
            }
        }
        Stat::Return { .. }
        | Stat::Expr { .. }
        | Stat::Local { .. }
        | Stat::Assign { .. }
        | Stat::CompoundAssign { .. }
        | Stat::Break { .. }
        | Stat::Continue { .. }
        | Stat::Function { .. }
        | Stat::LocalFunction { .. }
        | Stat::DeclareGlobal { .. }
        | Stat::DeclareFunction { .. }
        | Stat::DeclareClass { .. }
        | Stat::TypeAlias { .. }
        | Stat::TypeFunction { .. }
        | Stat::ClassProperty { .. } => {}
    }
}

fn generalize_local_function_query_types(
    root: &Stat,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    mode: AnalysisMode,
    query_types: &mut BTreeMap<LocalId, TypeId>,
) {
    if mode == AnalysisMode::NoCheck {
        return;
    }
    generalize_local_function_query_types_in_stat(root, dfg, arena, query_types);
}

fn widen_unannotated_singleton_query_types_in_stat(
    stat: &Stat,
    dfg: &DataFlowGraph,
    queries: &Queries,
    arena: &Arena,
    query_types: &mut BTreeMap<LocalId, TypeId>,
) {
    walk_query_stat_tree(stat, false, &mut |stat| {
        let Stat::Local { vars, values, .. } = stat else {
            return;
        };
        for (index, local) in vars.iter().enumerate() {
            if local.is_const || local.luau_type.is_some() {
                continue;
            }
            let Some(value) = values.get(index) else {
                continue;
            };
            if !query_initializer_widens_after_solve(value, queries, arena) {
                continue;
            }
            let Some(ty) = query_types
                .get(&local.id)
                .copied()
                .or_else(|| dfg.local(local.id).map(|def| dfg.get(def).ty))
            else {
                continue;
            };
            let widened = widen_singleton_query_type(arena, ty);
            if widened != arena.follow(ty) {
                query_types.insert(local.id, widened);
            }
        }
    });
}

fn query_initializer_widens_after_solve(expr: &Expr, queries: &Queries, arena: &Arena) -> bool {
    matches!(
        ungroup_expr(expr),
        Expr::IndexName { .. } | Expr::IndexExpr { .. }
    ) || matches!(
        ungroup_expr(expr),
        Expr::Call { func, args, .. }
            if query_call_result_widens_after_solve(func, args, queries, arena)
    )
}

fn query_call_result_widens_after_solve(
    func: &Expr,
    args: &[Expr],
    queries: &Queries,
    arena: &Arena,
) -> bool {
    // Bare literal locals already widen for by-name queries, but calls are
    // subtler: singleton-constrained generics and overloads intentionally
    // preserve literal returns. Only widen generic returns when the argument
    // flows through a primitive-widening parameter shape.
    let Some(callee) = queries.actual_by_syntax(func.syntax_id()) else {
        return false;
    };
    let TypeKind::Function(function) = arena.get(arena.follow(callee)) else {
        return false;
    };
    let Some(return_generic) = first_return_generic_name(arena, function.returns) else {
        return false;
    };
    let parameters = arena.normalize_pack(function.arguments);
    args.iter().zip(parameters.types).any(|(arg, parameter)| {
        matches!(ungroup_expr(arg), Expr::String { .. } | Expr::Bool { .. })
            && parameter_widens_return_generic_singleton(arena, parameter, &return_generic)
    })
}

fn first_return_generic_name(arena: &Arena, returns: TypePackId) -> Option<String> {
    let returns = arena.normalize_pack(returns);
    let ty = arena.follow(*returns.types.first()?);
    match arena.get(ty) {
        TypeKind::Generic(generic) => Some(generic.name.clone()),
        _ => None,
    }
}

fn parameter_widens_return_generic_singleton(
    arena: &Arena,
    parameter: TypeId,
    generic: &str,
) -> bool {
    let parameter = arena.follow(parameter);
    match arena.get(parameter) {
        TypeKind::Generic(candidate) => candidate.name == generic,
        TypeKind::Union(options) => {
            options
                .iter()
                .any(|option| type_is_generic_named(arena, *option, generic))
                && options
                    .iter()
                    .any(|option| type_is_primitive(arena, *option, PrimitiveType::Nil))
        }
        TypeKind::Intersection(options) => {
            options
                .iter()
                .any(|option| type_is_generic_named(arena, *option, generic))
                && options.iter().any(|option| {
                    type_is_primitive(arena, *option, PrimitiveType::String)
                        || type_is_primitive(arena, *option, PrimitiveType::Boolean)
                })
        }
        _ => false,
    }
}

fn type_is_generic_named(arena: &Arena, ty: TypeId, generic: &str) -> bool {
    matches!(
        arena.get(arena.follow(ty)),
        TypeKind::Generic(candidate) if candidate.name == generic
    )
}

fn type_is_primitive(arena: &Arena, ty: TypeId, primitive: PrimitiveType) -> bool {
    matches!(
        arena.get(arena.follow(ty)),
        TypeKind::Primitive(candidate) if *candidate == primitive
    )
}

fn widen_singleton_query_type(arena: &Arena, ty: TypeId) -> TypeId {
    match arena.get(arena.follow(ty)) {
        TypeKind::Singleton(SingletonType::Boolean(_)) => arena.primitives().boolean,
        TypeKind::Singleton(SingletonType::String(_)) => arena.primitives().string,
        TypeKind::Union(options) => {
            let mut primitive = None;
            for option in options {
                let option = arena.follow(*option);
                let current = match arena.get(option) {
                    TypeKind::Singleton(singleton) => singleton.primitive(),
                    TypeKind::Primitive(primitive) => *primitive,
                    _ => return ty,
                };
                if primitive.is_some_and(|primitive| primitive != current) {
                    return ty;
                }
                primitive = Some(current);
            }
            match primitive {
                Some(PrimitiveType::Boolean) => arena.primitives().boolean,
                Some(PrimitiveType::String) => arena.primitives().string,
                _ => ty,
            }
        }
        _ => ty,
    }
}

fn generalize_local_function_query_types_in_stat(
    stat: &Stat,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    query_types: &mut BTreeMap<LocalId, TypeId>,
) {
    walk_query_stat_tree(stat, false, &mut |stat| match stat {
        Stat::Local { vars, values, .. } => {
            for (index, local) in vars.iter().enumerate() {
                let Some(value) = values.get(index) else {
                    continue;
                };
                if local.luau_type.is_none()
                    && (expr_is_function_value(value)
                        || local_query_type_is_function(local.id, dfg, arena, query_types))
                {
                    generalize_local_query_type(local.id, dfg, arena, query_types);
                }
            }
        }
        Stat::Assign { vars, values, .. } => {
            for (var, value) in vars.iter().zip(values) {
                let Expr::Local { local, .. } = var else {
                    continue;
                };
                if expr_is_function_value(value) {
                    generalize_local_query_type(local.id, dfg, arena, query_types);
                }
            }
        }
        Stat::LocalFunction { name, .. } => {
            generalize_local_query_type(name.id, dfg, arena, query_types);
        }
        _ => {}
    });
}

fn local_query_type_is_function(
    local: LocalId,
    dfg: &DataFlowGraph,
    arena: &Arena,
    query_types: &BTreeMap<LocalId, TypeId>,
) -> bool {
    let Some(ty) = query_types
        .get(&local)
        .copied()
        .or_else(|| dfg.local(local).map(|def| dfg.get(def).ty))
    else {
        return false;
    };
    matches!(arena.get(arena.follow(ty)), TypeKind::Function(_))
}

fn specialize_unannotated_function_query_arguments(
    root: &Stat,
    arena: &mut Arena,
    global_defs: &mut BTreeMap<String, TypeId>,
    query_types: &mut BTreeMap<LocalId, TypeId>,
) {
    walk_query_stat_tree(root, true, &mut |stat| match stat {
        Stat::Function { name, func, .. } => {
            if let Expr::Global { name, .. } = name.as_ref()
                && let Some(ty) = global_defs.get_mut(name.as_str())
            {
                *ty = query_function_with_unknown_unannotated_arguments(func, arena, *ty);
            }
        }
        Stat::LocalFunction { name, func, .. } => {
            if let Some(ty) = query_types.get_mut(&name.id) {
                *ty = query_function_with_unknown_unannotated_arguments(func, arena, *ty);
            }
        }
        Stat::Local { vars, values, .. } => {
            for (index, local) in vars.iter().enumerate() {
                let Some(value) = values.get(index) else {
                    continue;
                };
                if let Some(ty) = query_types.get_mut(&local.id) {
                    *ty = query_function_with_unknown_unannotated_arguments(value, arena, *ty);
                }
            }
        }
        Stat::Assign { vars, values, .. } => {
            for (var, value) in vars.iter().zip(values) {
                let Expr::Local { local, .. } = var else {
                    continue;
                };
                if let Some(ty) = query_types.get_mut(&local.id) {
                    *ty = query_function_with_unknown_unannotated_arguments(value, arena, *ty);
                }
            }
        }
        _ => {}
    });
}

fn query_function_with_unknown_unannotated_arguments(
    func: &Expr,
    arena: &mut Arena,
    ty: TypeId,
) -> TypeId {
    let Expr::Function { args, self_arg, .. } = func else {
        return ty;
    };
    let TypeKind::Function(mut function) = arena.get(arena.follow(ty)).clone() else {
        return ty;
    };
    let TypePackKind::List { mut types, tail } = arena
        .get_pack(arena.follow_pack(function.arguments))
        .clone()
    else {
        return ty;
    };

    let mut parameters = Vec::with_capacity(args.len() + usize::from(self_arg.is_some()));
    if let Some(self_arg) = self_arg {
        parameters.push((self_arg.id, self_arg.luau_type.is_some()));
    }
    parameters.extend(args.iter().map(|arg| (arg.id, arg.luau_type.is_some())));
    let assigned_parameters = assigned_function_parameters(func);
    let input_read_parameters = input_read_function_parameters(func);
    let parameter_types = parameters
        .iter()
        .enumerate()
        .filter_map(|(index, (local, _))| types.get(index).copied().map(|ty| (*local, ty)))
        .collect::<BTreeMap<_, _>>();
    let any_callback_arguments =
        any_callback_argument_function_parameters(func, arena, &parameter_types);
    let return_constraints =
        return_constrained_function_parameters(func, arena, &function, &parameter_types);

    let mut changed = false;
    let mut removed_generics = Vec::new();
    for (index, (local, annotated)) in parameters.into_iter().enumerate() {
        if annotated || index >= types.len() {
            continue;
        }
        let arg_ty = types[index];
        if let Some(bound) = query_argument_known_bound(arena, arg_ty) {
            types[index] = bound;
            changed = true;
            continue;
        }
        if let Some((generic, primitive)) =
            query_argument_negated_singleton_primitive(arena, &function, arg_ty)
        {
            let replacement = match primitive {
                PrimitiveType::String => arena.primitives().string,
                PrimitiveType::Boolean => arena.primitives().boolean,
                _ => continue,
            };
            let instantiated = {
                let mut instantiator = crate::generalize::Instantiator::new(arena, TypeLevel(0));
                instantiator.bind_generic(&generic, replacement);
                instantiator.instantiate_type(ty)
            };
            return crate::normalize::simplify_type(arena, instantiated);
        }
        if let Some(constraints) = return_constraints.get(&local)
            && let Some(constrained) =
                intersect_query_argument_constraints(arena, arg_ty, constraints)
        {
            types[index] = constrained;
            changed = true;
            continue;
        }
        let has_generic_correlation =
            query_argument_has_generic_correlation(arena, &function, arg_ty);
        if !has_generic_correlation && any_callback_arguments.contains(&local) {
            types[index] = arena.primitives().any;
            changed = true;
            continue;
        }
        let incoming_value_is_discarded =
            assigned_parameters.contains(&local) && !input_read_parameters.contains(&local);
        if has_generic_correlation
            || (!incoming_value_is_discarded
                && !query_argument_is_uncorrelated_generic(arena, &function, arg_ty))
        {
            continue;
        }
        types[index] = arena.primitives().unknown;
        if let TypeKind::Generic(generic) = arena.get(arena.follow(arg_ty)) {
            removed_generics.push(generic.clone());
        }
        changed = true;
    }

    if !changed {
        if ensure_explicit_function_query_generics(func, &mut function) {
            return arena.alloc(TypeKind::Function(function));
        }
        return ty;
    }

    function.arguments = arena.alloc_pack(TypePackKind::List { types, tail });
    function
        .generics
        .retain(|generic| !removed_generics.contains(generic));
    ensure_explicit_function_query_generics(func, &mut function);
    arena.alloc(TypeKind::Function(function))
}

fn ensure_explicit_function_query_generics(
    func: &Expr,
    function: &mut crate::types::FunctionType,
) -> bool {
    let Expr::Function {
        generics,
        generic_packs,
        ..
    } = func
    else {
        return false;
    };

    let mut changed = false;
    for generic in generics {
        let generic = GenericType {
            name: generic.name.as_str().to_owned(),
            level: TypeLevel(0),
        };
        if !function.generics.contains(&generic) {
            function.generics.push(generic);
            changed = true;
        }
    }
    for generic_pack in generic_packs {
        let generic_pack = GenericTypePack {
            name: generic_pack.name.as_str().to_owned(),
            level: TypeLevel(0),
        };
        if !function.generic_packs.contains(&generic_pack) {
            function.generic_packs.push(generic_pack);
            changed = true;
        }
    }
    changed
}

// Query-only specialization for global function surfaces. If a lone generic
// parameter returns as itself minus string/boolean singletons, equality
// refinements have proven that callers see the primitive domain.
fn query_argument_negated_singleton_primitive(
    arena: &Arena,
    function: &crate::types::FunctionType,
    arg_ty: TypeId,
) -> Option<(crate::types::GenericType, PrimitiveType)> {
    let TypeKind::Generic(generic) = arena.get(arena.follow(arg_ty)).clone() else {
        return None;
    };
    if function.generics.len() != 1
        || !function.generic_packs.is_empty()
        || !function
            .generics
            .iter()
            .any(|candidate| candidate == &generic)
    {
        return None;
    }

    let mut primitive = None;
    let mut saw_constraint = false;
    for ty in arena.normalize_pack(function.returns).types {
        if !collect_negated_singleton_primitive_for_generic(
            arena,
            ty,
            &generic,
            &mut primitive,
            &mut saw_constraint,
            &mut BTreeSet::new(),
        ) {
            return None;
        }
    }
    saw_constraint.then_some((generic, primitive?))
}

fn collect_negated_singleton_primitive_for_generic(
    arena: &Arena,
    ty: TypeId,
    generic: &crate::types::GenericType,
    primitive: &mut Option<PrimitiveType>,
    saw_constraint: &mut bool,
    seen: &mut BTreeSet<TypeId>,
) -> bool {
    let ty = arena.follow(ty);
    if !seen.insert(ty) {
        return true;
    }
    match arena.get(ty) {
        TypeKind::Intersection(options) => {
            if options
                .iter()
                .any(|option| type_is_generic_parameter(arena, *option, generic))
            {
                for option in options {
                    if let Some(candidate) = negated_singleton_primitive(arena, *option) {
                        if primitive.is_some_and(|existing| existing != candidate) {
                            return false;
                        }
                        *primitive = Some(candidate);
                        *saw_constraint = true;
                    }
                }
            }
            options.iter().all(|option| {
                collect_negated_singleton_primitive_for_generic(
                    arena,
                    *option,
                    generic,
                    primitive,
                    saw_constraint,
                    seen,
                )
            })
        }
        TypeKind::Union(options) => options.iter().all(|option| {
            collect_negated_singleton_primitive_for_generic(
                arena,
                *option,
                generic,
                primitive,
                saw_constraint,
                seen,
            )
        }),
        TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
            collect_negated_singleton_primitive_for_generic(
                arena,
                *inner,
                generic,
                primitive,
                saw_constraint,
                seen,
            )
        }
        _ => true,
    }
}

fn type_is_generic_parameter(
    arena: &Arena,
    ty: TypeId,
    generic: &crate::types::GenericType,
) -> bool {
    matches!(
        arena.get(arena.follow(ty)),
        TypeKind::Generic(candidate) if candidate == generic
    )
}

fn negated_singleton_primitive(arena: &Arena, ty: TypeId) -> Option<PrimitiveType> {
    let TypeKind::Negation(target) = arena.get(arena.follow(ty)) else {
        return None;
    };
    let TypeKind::Singleton(singleton) = arena.get(arena.follow(*target)) else {
        return None;
    };
    match singleton.primitive() {
        PrimitiveType::String | PrimitiveType::Boolean => Some(singleton.primitive()),
        _ => None,
    }
}

fn query_argument_known_bound(arena: &Arena, arg_ty: TypeId) -> Option<TypeId> {
    if let TypeKind::Bound(bound) = arena.get(arg_ty) {
        return Some(arena.follow(*bound));
    }
    let TypeKind::Free(variable) = arena.get(arena.follow(arg_ty)) else {
        return None;
    };
    match (variable.lower_bound, variable.upper_bound) {
        (None, Some(upper_bound)) => Some(upper_bound),
        (Some(lower_bound), None) => Some(lower_bound),
        _ => None,
    }
}

fn return_constrained_function_parameters(
    func: &Expr,
    arena: &Arena,
    function: &crate::types::FunctionType,
    parameter_types: &BTreeMap<LocalId, TypeId>,
) -> BTreeMap<LocalId, Vec<TypeId>> {
    let Expr::Function {
        body,
        return_annotation: Some(_),
        ..
    } = func
    else {
        return BTreeMap::new();
    };
    let expected_returns = arena.normalize_pack(function.returns).types;
    if expected_returns.is_empty() {
        return BTreeMap::new();
    }
    let mut tracker = ReturnConstraintTracker {
        expected_returns,
        dependencies: parameter_types
            .keys()
            .map(|local| {
                (
                    *local,
                    ReturnDependency {
                        parameters: BTreeSet::from([*local]),
                        constrainable: true,
                    },
                )
            })
            .collect(),
        constraints: BTreeMap::new(),
    };
    tracker.scan_stat(body);
    tracker.constraints
}

fn intersect_query_argument_constraints(
    arena: &mut Arena,
    arg_ty: TypeId,
    constraints: &[TypeId],
) -> Option<TypeId> {
    let mut members = Vec::with_capacity(constraints.len() + 1);
    members.push(arg_ty);
    members.extend_from_slice(constraints);
    let intersection = arena.alloc(TypeKind::Intersection(members));
    let constrained = crate::normalize::simplify_type(arena, intersection);
    (constrained != arena.follow(arg_ty)
        && !matches!(arena.get(arena.follow(constrained)), TypeKind::Never))
    .then_some(constrained)
}

struct ReturnConstraintTracker {
    expected_returns: Vec<TypeId>,
    dependencies: BTreeMap<LocalId, ReturnDependency>,
    constraints: BTreeMap<LocalId, Vec<TypeId>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReturnDependency {
    parameters: BTreeSet<LocalId>,
    constrainable: bool,
}

impl ReturnConstraintTracker {
    fn scan_stat(&mut self, stat: &Stat) {
        match stat {
            Stat::Block { body, .. } => {
                for stat in body {
                    self.scan_stat(stat);
                }
            }
            Stat::Return { list, .. } => self.scan_return(list),
            Stat::Local { vars, values, .. } => {
                for (index, local) in vars.iter().enumerate() {
                    if let Some(mut dependency) = values
                        .get(index)
                        .and_then(|value| self.expr_dependencies(value))
                    {
                        dependency.constrainable = false;
                        self.dependencies.insert(local.id, dependency);
                    } else {
                        self.dependencies.remove(&local.id);
                    }
                }
            }
            Stat::Assign { vars, values, .. } => {
                let updates = vars
                    .iter()
                    .zip(values)
                    .filter_map(|(var, value)| {
                        let Expr::Local { local, .. } = ungroup_expr(var) else {
                            return None;
                        };
                        Some((local.id, self.expr_dependencies(value)))
                    })
                    .collect::<Vec<_>>();
                for (local, dependencies) in updates {
                    if let Some(mut dependencies) = dependencies {
                        dependencies.constrainable = true;
                        self.dependencies.insert(local, dependencies);
                    } else {
                        self.dependencies.remove(&local);
                    }
                }
            }
            Stat::If {
                then_body,
                else_body,
                ..
            } => {
                let before = self.dependencies.clone();
                let then_dependencies = self.branch_dependencies(then_body, &before);
                let else_dependencies = else_body.as_ref().map_or_else(
                    || before.clone(),
                    |else_body| self.branch_dependencies(else_body, &before),
                );
                self.dependencies =
                    merge_common_dependencies(&then_dependencies, &else_dependencies);
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::For { body, .. }
            | Stat::ForIn { body, .. } => self.scan_optional_body(body),
            Stat::Error { statements, .. }
            | Stat::Class {
                members: statements,
                ..
            } => {
                for stat in statements {
                    self.scan_stat(stat);
                }
            }
            Stat::Expr { .. }
            | Stat::CompoundAssign { .. }
            | Stat::Function { .. }
            | Stat::LocalFunction { .. }
            | Stat::Break { .. }
            | Stat::Continue { .. }
            | Stat::DeclareGlobal { .. }
            | Stat::DeclareFunction { .. }
            | Stat::DeclareClass { .. }
            | Stat::TypeAlias { .. }
            | Stat::TypeFunction { .. }
            | Stat::ClassProperty { .. } => {}
        }
    }

    fn scan_return(&mut self, list: &[Expr]) {
        let expected_returns = self.expected_returns.clone();
        for (expr, expected) in list.iter().zip(expected_returns) {
            let Some(dependency) = self.expr_dependencies(expr) else {
                continue;
            };
            if !dependency.constrainable {
                continue;
            }
            for parameter in dependency.parameters {
                self.constraints
                    .entry(parameter)
                    .or_default()
                    .push(expected);
            }
        }
    }

    fn branch_dependencies(
        &mut self,
        body: &Stat,
        before: &BTreeMap<LocalId, ReturnDependency>,
    ) -> BTreeMap<LocalId, ReturnDependency> {
        let outer = std::mem::replace(&mut self.dependencies, before.clone());
        self.scan_stat(body);
        let branch = self.dependencies.clone();
        self.dependencies = outer;
        branch
    }

    fn scan_optional_body(&mut self, body: &Stat) {
        let before = self.dependencies.clone();
        self.scan_stat(body);
        self.dependencies = before;
    }

    fn expr_dependencies(&self, expr: &Expr) -> Option<ReturnDependency> {
        match ungroup_expr(expr) {
            Expr::Local { local, .. } => self.dependencies.get(&local.id).cloned(),
            Expr::TypeAssertion { expr, .. } => self.expr_dependencies(expr),
            _ => None,
        }
    }
}

fn merge_common_dependencies(
    left: &BTreeMap<LocalId, ReturnDependency>,
    right: &BTreeMap<LocalId, ReturnDependency>,
) -> BTreeMap<LocalId, ReturnDependency> {
    left.iter()
        .filter_map(|(local, dependencies)| {
            (right.get(local) == Some(dependencies)).then_some((*local, dependencies.clone()))
        })
        .collect()
}

fn any_callback_argument_function_parameters(
    func: &Expr,
    arena: &Arena,
    parameter_types: &BTreeMap<LocalId, TypeId>,
) -> BTreeSet<LocalId> {
    let Expr::Function { body, .. } = func else {
        return BTreeSet::new();
    };
    let mut visitor = AnyCallbackArgumentVisitor {
        arena,
        parameter_types,
        any_arguments: BTreeSet::new(),
    };
    walk_stat(body, &mut visitor);
    visitor.any_arguments
}

struct AnyCallbackArgumentVisitor<'a> {
    arena: &'a Arena,
    parameter_types: &'a BTreeMap<LocalId, TypeId>,
    any_arguments: BTreeSet<LocalId>,
}

impl AnyCallbackArgumentVisitor<'_> {
    fn record_call(&mut self, func: &Expr, args: &[Expr]) {
        let Expr::Local { local: callee, .. } = ungroup_expr(func) else {
            return;
        };
        let Some(callee_ty) = self.parameter_types.get(&callee.id).copied() else {
            return;
        };
        let Some(parameter_types) = fixed_function_argument_types(self.arena, callee_ty) else {
            return;
        };
        for (arg, parameter_ty) in args.iter().zip(parameter_types) {
            if !matches!(
                self.arena.get(self.arena.follow(parameter_ty)),
                TypeKind::Any
            ) {
                continue;
            }
            let Expr::Local { local, .. } = ungroup_expr(arg) else {
                continue;
            };
            if self.parameter_types.contains_key(&local.id) {
                self.any_arguments.insert(local.id);
            }
        }
    }
}

impl Visitor<'_> for AnyCallbackArgumentVisitor<'_> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        match expr {
            Expr::Call { func, args, .. } => {
                self.record_call(func, args);
                WalkControl::Continue
            }
            Expr::Function { .. } => WalkControl::SkipChildren,
            _ => WalkControl::Continue,
        }
    }
}

fn fixed_function_argument_types(arena: &Arena, ty: TypeId) -> Option<Vec<TypeId>> {
    let TypeKind::Function(function) = arena.get(arena.follow(ty)) else {
        return None;
    };
    let TypePackKind::List { types, tail: None } =
        arena.get_pack(arena.follow_pack(function.arguments))
    else {
        return None;
    };
    Some(types.clone())
}

fn input_read_function_parameters(func: &Expr) -> BTreeSet<LocalId> {
    let Expr::Function {
        args,
        self_arg,
        body,
        ..
    } = func
    else {
        return BTreeSet::new();
    };
    let parameters = self_arg
        .iter()
        .chain(args.iter())
        .map(|local| local.id)
        .collect::<BTreeSet<_>>();
    let mut tracker = ParameterInputReadTracker {
        parameters,
        definitely_assigned: BTreeSet::new(),
        read_before_assignment: BTreeSet::new(),
    };
    tracker.scan_stat(body);
    tracker.read_before_assignment
}

struct ParameterInputReadTracker {
    parameters: BTreeSet<LocalId>,
    definitely_assigned: BTreeSet<LocalId>,
    read_before_assignment: BTreeSet<LocalId>,
}

impl ParameterInputReadTracker {
    fn scan_stat(&mut self, stat: &Stat) {
        match stat {
            Stat::Block { body, .. } => {
                for stat in body {
                    self.scan_stat(stat);
                }
            }
            Stat::Return { list, .. } => self.scan_exprs(list),
            Stat::Expr { expr, .. } => self.scan_expr(expr),
            Stat::Local { values, .. } => self.scan_exprs(values),
            Stat::Assign { vars, values, .. } => {
                self.scan_exprs(values);
                for var in vars {
                    self.scan_assignment_target(var);
                }
                for var in vars {
                    self.mark_assignment_target(var);
                }
            }
            Stat::CompoundAssign { var, value, .. } => {
                self.scan_expr(var);
                self.scan_expr(value);
                self.mark_assignment_target(var);
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.scan_expr(condition);
                let before = self.definitely_assigned.clone();
                let then_assigned = self.branch_definitely_assigned(then_body, &before);
                let else_assigned = else_body.as_ref().map_or_else(
                    || before.clone(),
                    |else_body| self.branch_definitely_assigned(else_body, &before),
                );
                self.definitely_assigned = then_assigned
                    .intersection(&else_assigned)
                    .copied()
                    .collect();
            }
            Stat::While {
                condition, body, ..
            } => {
                self.scan_expr(condition);
                self.scan_optional_body(body);
            }
            Stat::Repeat {
                condition, body, ..
            } => {
                self.scan_optional_body(body);
                self.scan_expr(condition);
            }
            Stat::For {
                from,
                to,
                step,
                body,
                ..
            } => {
                self.scan_expr(from);
                self.scan_expr(to);
                if let Some(step) = step {
                    self.scan_expr(step);
                }
                self.scan_optional_body(body);
            }
            Stat::ForIn { values, body, .. } => {
                self.scan_exprs(values);
                self.scan_optional_body(body);
            }
            Stat::Error { statements, .. }
            | Stat::Class {
                members: statements,
                ..
            } => {
                for stat in statements {
                    self.scan_stat(stat);
                }
            }
            Stat::Function { name, .. } => self.scan_assignment_target(name),
            Stat::LocalFunction { .. }
            | Stat::Break { .. }
            | Stat::Continue { .. }
            | Stat::DeclareGlobal { .. }
            | Stat::DeclareFunction { .. }
            | Stat::DeclareClass { .. }
            | Stat::TypeAlias { .. }
            | Stat::TypeFunction { .. }
            | Stat::ClassProperty { .. } => {}
        }
    }

    fn branch_definitely_assigned(
        &mut self,
        body: &Stat,
        before: &BTreeSet<LocalId>,
    ) -> BTreeSet<LocalId> {
        let outer = std::mem::replace(&mut self.definitely_assigned, before.clone());
        self.scan_stat(body);
        let branch = self.definitely_assigned.clone();
        self.definitely_assigned = outer;
        branch
    }

    fn scan_optional_body(&mut self, body: &Stat) {
        let before = self.definitely_assigned.clone();
        self.scan_stat(body);
        self.definitely_assigned = before;
    }

    fn scan_exprs(&mut self, expressions: &[Expr]) {
        for expr in expressions {
            self.scan_expr(expr);
        }
    }

    fn scan_expr(&mut self, expr: &Expr) {
        let mut visitor = ParameterInputReadVisitor {
            parameters: &self.parameters,
            definitely_assigned: &self.definitely_assigned,
            read_before_assignment: &mut self.read_before_assignment,
        };
        ruau_ast::visit::walk_expr(expr, &mut visitor);
    }

    fn scan_assignment_target(&mut self, expr: &Expr) {
        match ungroup_expr(expr) {
            Expr::Local { .. } => {}
            Expr::IndexName { expr, .. } => self.scan_expr(expr),
            Expr::IndexExpr { expr, index, .. } => {
                self.scan_expr(expr);
                self.scan_expr(index);
            }
            other => self.scan_expr(other),
        }
    }

    fn mark_assignment_target(&mut self, expr: &Expr) {
        if let Expr::Local { local, .. } = ungroup_expr(expr)
            && self.parameters.contains(&local.id)
        {
            self.definitely_assigned.insert(local.id);
        }
    }
}

struct ParameterInputReadVisitor<'a> {
    parameters: &'a BTreeSet<LocalId>,
    definitely_assigned: &'a BTreeSet<LocalId>,
    read_before_assignment: &'a mut BTreeSet<LocalId>,
}

impl Visitor<'_> for ParameterInputReadVisitor<'_> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        match expr {
            Expr::Local { local, .. }
                if self.parameters.contains(&local.id)
                    && !self.definitely_assigned.contains(&local.id) =>
            {
                self.read_before_assignment.insert(local.id);
                WalkControl::Continue
            }
            Expr::Function { .. } => WalkControl::SkipChildren,
            _ => WalkControl::Continue,
        }
    }
}

fn assigned_function_parameters(func: &Expr) -> BTreeSet<LocalId> {
    let Expr::Function {
        args,
        self_arg,
        body,
        ..
    } = func
    else {
        return BTreeSet::new();
    };
    let parameters = self_arg
        .iter()
        .chain(args.iter())
        .map(|local| local.id)
        .collect::<BTreeSet<_>>();
    let mut assigned = BTreeSet::new();
    collect_assigned_parameters(body, &parameters, &mut assigned);
    assigned
}

fn collect_assigned_parameters(
    stat: &Stat,
    parameters: &BTreeSet<LocalId>,
    assigned: &mut BTreeSet<LocalId>,
) {
    match stat {
        Stat::Block { body, .. } => {
            for stat in body {
                collect_assigned_parameters(stat, parameters, assigned);
            }
        }
        Stat::Assign { vars, .. } => {
            for var in vars {
                if let Expr::Local { local, .. } = var
                    && parameters.contains(&local.id)
                {
                    assigned.insert(local.id);
                }
            }
        }
        Stat::CompoundAssign { var, .. } => {
            if let Expr::Local { local, .. } = var.as_ref()
                && parameters.contains(&local.id)
            {
                assigned.insert(local.id);
            }
        }
        Stat::If {
            then_body,
            else_body,
            ..
        } => {
            collect_assigned_parameters(then_body, parameters, assigned);
            if let Some(else_body) = else_body {
                collect_assigned_parameters(else_body, parameters, assigned);
            }
        }
        Stat::While { body, .. }
        | Stat::Repeat { body, .. }
        | Stat::For { body, .. }
        | Stat::ForIn { body, .. } => collect_assigned_parameters(body, parameters, assigned),
        Stat::Error { statements, .. }
        | Stat::Class {
            members: statements,
            ..
        } => {
            for stat in statements {
                collect_assigned_parameters(stat, parameters, assigned);
            }
        }
        Stat::Local { .. }
        | Stat::Return { .. }
        | Stat::Expr { .. }
        | Stat::Break { .. }
        | Stat::Continue { .. }
        | Stat::Function { .. }
        | Stat::LocalFunction { .. }
        | Stat::DeclareGlobal { .. }
        | Stat::DeclareFunction { .. }
        | Stat::DeclareClass { .. }
        | Stat::TypeAlias { .. }
        | Stat::TypeFunction { .. }
        | Stat::ClassProperty { .. } => {}
    }
}

fn query_argument_is_uncorrelated_generic(
    arena: &Arena,
    function: &crate::types::FunctionType,
    arg_ty: TypeId,
) -> bool {
    matches!(
        arena.get(arena.follow(arg_ty)),
        TypeKind::Free(_) | TypeKind::Generic(_)
    ) && TypeOccurrenceCounter::new(arena, arg_ty).count_in_pack(function.arguments) == 1
        && !TypeOccurrenceCounter::new(arena, arg_ty).occurs_in_pack(function.returns)
}

fn query_argument_has_generic_correlation(
    arena: &Arena,
    function: &crate::types::FunctionType,
    arg_ty: TypeId,
) -> bool {
    if !matches!(
        arena.get(arena.follow(arg_ty)),
        TypeKind::Free(_) | TypeKind::Generic(_)
    ) {
        return false;
    }
    TypeOccurrenceCounter::new(arena, arg_ty).count_in_pack(function.arguments) > 1
        || TypeOccurrenceCounter::new(arena, arg_ty).occurs_in_pack(function.returns)
}

struct TypeOccurrenceCounter<'a> {
    arena: &'a Arena,
    needle: TypeId,
    seen_types: BTreeSet<TypeId>,
    seen_packs: BTreeSet<TypePackId>,
}

impl<'a> TypeOccurrenceCounter<'a> {
    fn new(arena: &'a Arena, needle: TypeId) -> Self {
        Self {
            arena,
            needle,
            seen_types: BTreeSet::new(),
            seen_packs: BTreeSet::new(),
        }
    }

    fn occurs_in_pack(mut self, pack: TypePackId) -> bool {
        self.count_in_pack(pack) > 0
    }

    fn count_in_pack(&mut self, pack: TypePackId) -> usize {
        let pack = self.arena.follow_pack(pack);
        if !self.seen_packs.insert(pack) {
            return 0;
        }
        match self.arena.get_pack(pack) {
            TypePackKind::List { types, tail } => {
                types
                    .iter()
                    .map(|ty| self.count_in_type(*ty))
                    .sum::<usize>()
                    + tail.map(|tail| self.count_in_pack(tail)).unwrap_or(0)
            }
            TypePackKind::Variadic { ty } => self.count_in_type(*ty),
            TypePackKind::Bound(bound) => self.count_in_pack(*bound),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => 0,
        }
    }

    fn count_in_type(&mut self, ty: TypeId) -> usize {
        if ty == self.needle {
            return 1;
        }
        let ty = self.arena.follow(ty);
        if !self.seen_types.insert(ty) {
            return 0;
        }
        match self.arena.get(ty) {
            TypeKind::Function(function) => {
                self.count_in_pack(function.arguments) + self.count_in_pack(function.returns)
            }
            TypeKind::Table(table) => {
                table
                    .properties
                    .values()
                    .map(|property| {
                        self.count_in_type(property.ty)
                            + property
                                .write_ty
                                .map(|ty| self.count_in_type(ty))
                                .unwrap_or(0)
                    })
                    .sum::<usize>()
                    + table
                        .indexer
                        .as_ref()
                        .map(|indexer| {
                            self.count_in_type(indexer.key) + self.count_in_type(indexer.value)
                        })
                        .unwrap_or(0)
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => self.count_in_type(*table) + self.count_in_type(*metatable),
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => {
                arguments.iter().map(|ty| self.count_in_type(*ty)).sum()
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => self.count_in_type(*inner),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => 0,
        }
    }
}

fn generalize_local_query_type(
    local: LocalId,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    query_types: &mut BTreeMap<LocalId, TypeId>,
) {
    let Some(def) = dfg.local(local) else {
        return;
    };
    let preserve_nil = query_types
        .get(&local)
        .is_some_and(|ty| arena.may_be_nil(*ty));
    let ty = dfg.get(def).ty;
    let query_ty = if function_query_return_contains_type_function(arena, ty) {
        crate::generalize::generalize_function_frees(arena, ty)
    } else {
        crate::generalize::generalize_function_frees_to_unknown(arena, ty)
    };
    let mut query_ty = crate::generalize::resolve_function_free_bounds_for_query(arena, query_ty);
    if preserve_nil && !arena.may_be_nil(query_ty) {
        let nil = arena.primitives().nil;
        let optional = arena.alloc(TypeKind::Union(vec![nil, query_ty]));
        query_ty = crate::normalize::simplify_type(arena, optional);
    }
    query_types.insert(local, query_ty);
}

fn function_query_return_contains_type_function(arena: &Arena, ty: TypeId) -> bool {
    let TypeKind::Function(function) = arena.get(arena.follow(ty)) else {
        return false;
    };
    pack_contains_type_function(
        arena,
        function.returns,
        &mut BTreeSet::new(),
        &mut BTreeSet::new(),
    )
}

fn pack_contains_type_function(
    arena: &Arena,
    pack: TypePackId,
    seen_types: &mut BTreeSet<TypeId>,
    seen_packs: &mut BTreeSet<TypePackId>,
) -> bool {
    let pack = arena.follow_pack(pack);
    if !seen_packs.insert(pack) {
        return false;
    }
    match arena.get_pack(pack) {
        TypePackKind::List { types, tail } => {
            types
                .iter()
                .any(|ty| type_contains_type_function(arena, *ty, seen_types, seen_packs))
                || tail.is_some_and(|tail| {
                    pack_contains_type_function(arena, tail, seen_types, seen_packs)
                })
        }
        TypePackKind::Variadic { ty } => {
            type_contains_type_function(arena, *ty, seen_types, seen_packs)
        }
        TypePackKind::Bound(bound) => {
            pack_contains_type_function(arena, *bound, seen_types, seen_packs)
        }
        TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
    }
}

fn type_contains_type_function(
    arena: &Arena,
    ty: TypeId,
    seen_types: &mut BTreeSet<TypeId>,
    seen_packs: &mut BTreeSet<TypePackId>,
) -> bool {
    let ty = arena.follow(ty);
    if !seen_types.insert(ty) {
        return false;
    }
    match arena.get(ty) {
        TypeKind::TypeFunctionInstance { .. } => true,
        TypeKind::Function(function) => {
            pack_contains_type_function(arena, function.arguments, seen_types, seen_packs)
                || pack_contains_type_function(arena, function.returns, seen_types, seen_packs)
        }
        TypeKind::Table(table) => {
            table.properties.values().any(|property| {
                type_contains_type_function(arena, property.ty, seen_types, seen_packs)
                    || property.write_ty.is_some_and(|ty| {
                        type_contains_type_function(arena, ty, seen_types, seen_packs)
                    })
            }) || table.indexer.as_ref().is_some_and(|indexer| {
                type_contains_type_function(arena, indexer.key, seen_types, seen_packs)
                    || type_contains_type_function(arena, indexer.value, seen_types, seen_packs)
            })
        }
        TypeKind::Metatable {
            table, metatable, ..
        } => {
            type_contains_type_function(arena, *table, seen_types, seen_packs)
                || type_contains_type_function(arena, *metatable, seen_types, seen_packs)
        }
        TypeKind::Union(types) | TypeKind::Intersection(types) => types
            .iter()
            .any(|ty| type_contains_type_function(arena, *ty, seen_types, seen_packs)),
        TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
            type_contains_type_function(arena, *inner, seen_types, seen_packs)
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

fn expr_is_function_value(expr: &Expr) -> bool {
    match expr {
        Expr::Function { .. } => true,
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => expr_is_function_value(expr),
        _ => false,
    }
}
