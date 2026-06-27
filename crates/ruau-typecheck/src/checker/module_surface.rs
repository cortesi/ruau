use std::collections::BTreeSet;

use ruau_analysis::resolve::AnalysisMode;
use ruau_ast::{
    json::JsonTableItemKind,
    syntax::{Expr, Stat},
};

use super::{ExportedType, ExportedTypeKind, ModuleExports};
use crate::{
    annotation::lower_non_generic_type_alias_annotation,
    dfg::DataFlowGraph,
    diagnostic::{DiagnosticCategory, DiagnosticLocation, TypeDiagnostic},
    queries::Queries,
    scopes::{ScopeTree, TypeBindingKind},
    types::{Arena, TableProperty, TableState, TableType, TypeId, TypeKind},
};

pub(super) fn collect_exports(
    scopes: &ScopeTree,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    mode: AnalysisMode,
) -> ModuleExports {
    let types = scopes
        .get(scopes.root())
        .type_bindings
        .iter()
        .filter(|&(_, binding)| binding.exported && binding.kind != TypeBindingKind::BuiltinType)
        .map(|(name, binding)| {
            let ty = binding.ty.or_else(|| {
                (!binding.alias_has_generics)
                    .then_some(binding.alias.as_ref())
                    .flatten()
                    .map(|alias| {
                        let alias_identity = binding
                            .alias_identity
                            .clone()
                            .unwrap_or_else(|| scopes.alias_identity(scopes.root(), name));
                        let (ty, _) = lower_non_generic_type_alias_annotation(
                            &binding.name,
                            alias_identity,
                            alias,
                            scopes,
                            dfg,
                            arena,
                            mode,
                        );
                        ty
                    })
            });
            (
                name.clone(),
                ExportedType {
                    name: binding.name.clone(),
                    alias_identity: binding.alias_identity.clone(),
                    kind: ExportedTypeKind::from(binding.kind),
                    ty,
                    alias: binding.alias.clone(),
                    alias_has_generics: binding.alias_has_generics,
                    generic_names: binding.generic_names.clone(),
                    generic_locations: binding.generic_locations.clone(),
                    generic_defaults: binding.generic_defaults.clone(),
                    generic_pack_names: binding.generic_pack_names.clone(),
                    generic_pack_locations: binding.generic_pack_locations.clone(),
                    generic_pack_defaults: binding.generic_pack_defaults.clone(),
                },
            )
        })
        .collect();
    ModuleExports { types }
}

/// Primitive builtin type names that a user type alias or type function may
/// not redefine. Mirrors the primitive set installed by [`crate::builtins::BuiltinEnvironment`];
/// extern / declared classes are deliberately excluded, since shadowing those
/// with a local alias is allowed.
const PRIMITIVE_BUILTIN_TYPE_NAMES: &[&str] = &[
    "nil", "boolean", "number", "string", "thread", "buffer", "vector", "any", "unknown", "never",
];

/// Type-definition names reserved by the language: a type alias or type
/// function may not use them as its name.
const RESERVED_TYPE_DEFINITION_NAMES: &[&str] = &["typeof"];

/// Reports duplicate and reserved type-definition declarations by walking the
/// module AST. Type aliases and type functions are block-scoped and hoisted, so
/// two definitions of the same name within one lexical block are a duplicate,
/// as is redefining a primitive builtin type. Shadowing across nested blocks,
/// and shadowing an extern / declared class, is allowed.
pub(super) fn type_definition_issue_diagnostics(root: &Stat) -> Vec<TypeDiagnostic> {
    let mut diagnostics = Vec::new();
    check_block_type_definitions(root, &mut diagnostics);
    diagnostics
}

fn check_block_type_definitions(stat: &Stat, diagnostics: &mut Vec<TypeDiagnostic>) {
    match stat {
        Stat::Block { body, .. } => {
            let mut seen = BTreeSet::new();
            for child in body {
                if let Stat::TypeAlias { name, location, .. }
                | Stat::TypeFunction { name, location, .. } = child
                {
                    let first_in_block = seen.insert(name.as_str());
                    diagnostics.extend(type_definition_issue(
                        name.as_str(),
                        *location,
                        first_in_block,
                    ));
                }
                check_block_type_definitions(child, diagnostics);
            }
        }
        Stat::If {
            then_body,
            else_body,
            ..
        } => {
            check_block_type_definitions(then_body, diagnostics);
            if let Some(else_body) = else_body {
                check_block_type_definitions(else_body, diagnostics);
            }
        }
        Stat::While { body, .. }
        | Stat::Repeat { body, .. }
        | Stat::For { body, .. }
        | Stat::ForIn { body, .. } => check_block_type_definitions(body, diagnostics),
        Stat::Function { func, .. } | Stat::LocalFunction { func, .. } => {
            if let Expr::Function { body, .. } = func.as_ref() {
                check_block_type_definitions(body, diagnostics);
            }
        }
        Stat::Error { statements, .. } => {
            for child in statements {
                check_block_type_definitions(child, diagnostics);
            }
        }
        _ => {}
    }
}

fn type_definition_issue(
    name: &str,
    location: Option<ruau_ast::Location>,
    first_in_block: bool,
) -> Option<TypeDiagnostic> {
    let diagnostic_location = DiagnosticLocation::from_opt(location);
    if RESERVED_TYPE_DEFINITION_NAMES.contains(&name) {
        return Some(
            TypeDiagnostic::error(DiagnosticCategory::Resolver, diagnostic_location)
                .with_context(format!(
                    "Type identifier '{name}' is reserved and cannot name a type alias or type function"
                ))
                .with_typed(crate::diagnostic::Payload::ReservedTypeIdentifier {
                    name: name.to_owned(),
                }),
        );
    }
    if !first_in_block || PRIMITIVE_BUILTIN_TYPE_NAMES.contains(&name) {
        return Some(
            TypeDiagnostic::error(DiagnosticCategory::Resolver, diagnostic_location)
                .with_context(format!("Redefinition of type '{name}'"))
                .with_typed(crate::diagnostic::Payload::DuplicateTypeDefinition {
                    name: name.to_owned(),
                }),
        );
    }
    None
}

pub(super) fn collect_module_return_types(
    root: &Stat,
    mode: AnalysisMode,
    queries: &Queries,
    arena: &mut Arena,
) -> Vec<TypeId> {
    root_return_exprs(root)
        .into_iter()
        .map(|expr| match mode {
            AnalysisMode::NoCheck => unchecked_return_type(expr, arena),
            AnalysisMode::Strict | AnalysisMode::Nonstrict => queries
                .actual_by_syntax(expr.syntax_id())
                .unwrap_or_else(|| arena.primitives().any),
        })
        .collect()
}

fn root_return_exprs(root: &Stat) -> Vec<&Expr> {
    match root {
        Stat::Block { body, .. } => body
            .iter()
            .filter_map(|stat| match stat {
                Stat::Return { list, .. } => Some(list.as_slice()),
                _ => None,
            })
            .flatten()
            .collect(),
        Stat::Return { list, .. } => list.iter().collect(),
        _ => Vec::new(),
    }
}

fn unchecked_return_type(expr: &Expr, arena: &mut Arena) -> TypeId {
    match expr {
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
            unchecked_return_type(expr, arena)
        }
        Expr::Table { items, .. } => {
            let mut table = TableType::new(TableState::Sealed);
            for item in items {
                let ty = match &item.value {
                    Expr::Table { .. } => unchecked_return_type(&item.value, arena),
                    _ => arena.primitives().any,
                };
                match (&item.kind, &item.key) {
                    (
                        JsonTableItemKind::Record | JsonTableItemKind::General,
                        Some(Expr::String {
                            value, location, ..
                        }),
                    ) => {
                        table.properties.insert(
                            value.clone(),
                            TableProperty::new(ty)
                                .with_location(location.map(DiagnosticLocation::from)),
                        );
                    }
                    (JsonTableItemKind::Record, Some(Expr::Global { name, location, .. })) => {
                        table.properties.insert(
                            name.as_str().to_owned(),
                            TableProperty::new(ty)
                                .with_location(location.map(DiagnosticLocation::from)),
                        );
                    }
                    _ => {}
                }
            }
            arena.alloc(TypeKind::Table(table))
        }
        _ => arena.primitives().any,
    }
}
