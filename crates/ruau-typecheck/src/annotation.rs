//! Standalone type-annotation lowering entry points.
//!
//! These wrappers keep query/checker surfaces from depending directly on the
//! expression-constraint generation module graph.

use std::collections::BTreeMap;

use ruau_analysis::resolve::AnalysisMode;
use ruau_ast::syntax::Type;

use crate::{
    dfg::DataFlowGraph,
    diagnostics::Diagnostics,
    generation::{GenerationConfig, state::ExpressionConstraintGenerator},
    scopes::ScopeTree,
    types::{Arena, TableAliasIdentity, TypeId, TypeKind},
};

pub fn lower_type_annotation(
    ty: &Type,
    scopes: &ScopeTree,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    mode: AnalysisMode,
) -> (TypeId, Diagnostics) {
    lower_type_annotation_with_globals(ty, scopes, dfg, arena, mode, &BTreeMap::new())
}

pub fn lower_non_generic_type_alias_annotation(
    alias_name: &str,
    alias_identity: TableAliasIdentity,
    alias: &Type,
    scopes: &ScopeTree,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    mode: AnalysisMode,
) -> (TypeId, Diagnostics) {
    let empty_require_returns = BTreeMap::new();
    let mut generator = ExpressionConstraintGenerator::new(
        scopes,
        dfg,
        arena,
        mode,
        GenerationConfig::default(),
        &empty_require_returns,
    );
    let ty = generator.lower_non_generic_alias(scopes.root(), alias_name, alias_identity, alias);
    let followed = generator.arena.follow(ty);
    let export_ty = match generator.arena.get(followed).clone() {
        TypeKind::Table(mut table) => {
            table.name = None;
            generator.arena.replace(followed, TypeKind::Table(table));
            ty
        }
        TypeKind::Metatable { .. } => ty,
        _ => followed,
    };
    generator.assert_frame_stacks_empty();
    (export_ty, generator.generated.diagnostics)
}

pub fn lower_type_annotation_with_globals(
    ty: &Type,
    scopes: &ScopeTree,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    mode: AnalysisMode,
    global_defs: &BTreeMap<String, TypeId>,
) -> (TypeId, Diagnostics) {
    let empty_require_returns = BTreeMap::new();
    let mut generator = ExpressionConstraintGenerator::new(
        scopes,
        dfg,
        arena,
        mode,
        GenerationConfig::default(),
        &empty_require_returns,
    );
    generator.generated.global_defs.clone_from(global_defs);
    let ty = generator.lower_type(scopes.root(), ty);
    generator.assert_frame_stacks_empty();
    (ty, generator.generated.diagnostics)
}
