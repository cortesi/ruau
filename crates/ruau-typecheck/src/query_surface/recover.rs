use std::collections::BTreeMap;

use ruau_analysis::AnalysisMode;
use ruau_ast::syntax::{Expr, LocalId, Stat, Type};

use super::walk_query_stat_tree;
use crate::{
    annotation::lower_type_annotation,
    dfg::DataFlowGraph,
    scopes::ScopeTree,
    types::{Arena, TypeId, TypeKind},
};

pub fn recover_nocheck_query_local_types(
    root: &Stat,
    scopes: &ScopeTree,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
) -> BTreeMap<LocalId, TypeId> {
    let mut query_types = BTreeMap::new();
    walk_query_stat_tree(root, false, &mut |stat| {
        let Stat::Local { vars, values, .. } = stat else {
            return;
        };
        for (index, local) in vars.iter().enumerate() {
            let recovered = local
                .annotation
                .as_deref()
                .map(|annotation| recover_nocheck_annotation_type(annotation, scopes, dfg, arena))
                .or_else(|| {
                    values.get(index).and_then(|value| {
                        recover_nocheck_expr_query_type(value, arena, &query_types)
                    })
                });
            if let Some(ty) = recovered {
                query_types.insert(local.id, ty);
            }
        }
    });
    query_types
}

fn recover_nocheck_annotation_type(
    annotation: &Type,
    scopes: &ScopeTree,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
) -> TypeId {
    let (ty, _) = lower_type_annotation(annotation, scopes, dfg, arena, AnalysisMode::NoCheck);
    ty
}

fn recover_nocheck_expr_query_type(
    expr: &Expr,
    arena: &Arena,
    query_types: &BTreeMap<LocalId, TypeId>,
) -> Option<TypeId> {
    match expr {
        Expr::Nil { .. } => Some(arena.primitives().nil),
        Expr::Bool { .. } => Some(arena.primitives().boolean),
        Expr::Number { .. } | Expr::Integer { .. } => Some(arena.primitives().number),
        Expr::String { .. } => Some(arena.primitives().string),
        Expr::Local { local, .. } => query_types.get(&local.id).copied(),
        Expr::Error { .. } => Some(arena.primitives().error),
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
            recover_nocheck_expr_query_type(expr, arena, query_types)
        }
        Expr::IndexName { expr, .. } | Expr::IndexExpr { expr, .. } => {
            let base = recover_nocheck_expr_query_type(expr, arena, query_types)?;
            matches!(arena.get(arena.follow(base)), TypeKind::Error)
                .then_some(arena.primitives().error)
        }
        _ => None,
    }
}
