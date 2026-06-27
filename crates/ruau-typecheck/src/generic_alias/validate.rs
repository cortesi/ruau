//! Root type-alias validation and materialization diagnostics.
//!
//! These run after binding population: they validate generic-alias reference
//! shapes, detect transparent recursive cycles and duplicate generic
//! parameters, and materialize non-generic root aliases into arena types.

use std::collections::{BTreeMap, BTreeSet};

use ruau_analysis::resolve::AnalysisMode;
use ruau_ast::{
    syntax::{Expr, Type, TypePack, TypeParameter},
    visit::{NodePath, Visitor, WalkControl, walk_type, walk_type_pack},
};

use super::shape::{
    argument_counts, arguments_are_out_of_order, type_argument_can_follow_pack,
    type_reference_has_parameter_list,
};
use crate::{
    annotation::lower_type_annotation_with_globals,
    dfg::DataFlowGraph,
    diagnostic::{DiagnosticCategory, DiagnosticLocation, TypeDiagnostic},
    scopes::{ScopeId, ScopeTree, TypeBinding},
    types::{Arena, TypeId, TypeKind},
};

pub fn validate_root_type_aliases(
    scopes: &ScopeTree,
    global_defs: &BTreeMap<String, TypeId>,
) -> Vec<TypeDiagnostic> {
    let root = scopes.root();
    let mut diagnostics = Vec::new();
    let mut reported_recursive_aliases = BTreeSet::new();
    for (name, binding) in &scopes.get(root).type_bindings {
        if binding.alias.is_none() {
            continue;
        }
        diagnostics.extend(duplicate_generic_name_diagnostics(binding));
        diagnostics.extend(generic_alias_default_diagnostics(
            scopes,
            root,
            global_defs,
            binding,
        ));
        if let Some(alias) = binding.alias.as_ref() {
            diagnostics.extend(generic_alias_body_diagnostics(scopes, root, binding, alias));
        }
        if reported_recursive_aliases.contains(name) {
            continue;
        }
        if let Some(alias) = binding.alias.as_ref() {
            let mut stack = vec![name.clone()];
            if alias_has_transparent_cycle(scopes, root, name, binding, alias, &mut stack) {
                reported_recursive_aliases.extend(stack.iter().cloned());
                diagnostics.push(recursive_type_alias_diagnostic(name, type_location(alias)));
            }
        }
    }
    diagnostics
}

fn generic_alias_default_diagnostics(
    scopes: &ScopeTree,
    scope: ScopeId,
    global_defs: &BTreeMap<String, TypeId>,
    binding: &TypeBinding,
) -> Vec<TypeDiagnostic> {
    let mut visitor = TypeofDefaultVisitor::new(scopes, scope, global_defs);
    for default in binding.generic_defaults.iter().flatten() {
        walk_type(default, &mut visitor);
    }
    for default in binding.generic_pack_defaults.iter().flatten() {
        walk_type_pack(default, &mut visitor);
    }
    visitor.into_diagnostics()
}

fn generic_alias_body_diagnostics(
    scopes: &ScopeTree,
    scope: ScopeId,
    current_binding: &TypeBinding,
    ty: &Type,
) -> Vec<TypeDiagnostic> {
    let mut diagnostics = Vec::new();
    collect_generic_alias_type_diagnostics(scopes, scope, current_binding, ty, &mut diagnostics);
    diagnostics
}

struct TypeofDefaultVisitor<'a> {
    scopes: &'a ScopeTree,
    scope: ScopeId,
    global_defs: &'a BTreeMap<String, TypeId>,
    diagnostics: Vec<TypeDiagnostic>,
}

impl<'a> TypeofDefaultVisitor<'a> {
    fn new(
        scopes: &'a ScopeTree,
        scope: ScopeId,
        global_defs: &'a BTreeMap<String, TypeId>,
    ) -> Self {
        Self {
            scopes,
            scope,
            global_defs,
            diagnostics: Vec::new(),
        }
    }

    fn into_diagnostics(self) -> Vec<TypeDiagnostic> {
        self.diagnostics
    }
}

impl Visitor<'_> for TypeofDefaultVisitor<'_> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &Expr) -> WalkControl {
        match expr {
            Expr::Global { location, name, .. }
                if self
                    .scopes
                    .lookup_global(self.scope, name.as_str())
                    .is_none()
                    && !self.global_defs.contains_key(name.as_str()) =>
            {
                self.diagnostics.push(TypeDiagnostic::unknown_symbol(
                    name.as_str(),
                    DiagnosticLocation::from_opt(*location),
                ));
            }
            Expr::Local {
                location, local, ..
            } => {
                if let (Some(reference), Some(binding)) = (*location, local.location)
                    && binding.begin > reference.begin
                {
                    self.diagnostics.push(TypeDiagnostic::unknown_symbol(
                        local.name.as_str(),
                        DiagnosticLocation::from(reference),
                    ));
                }
            }
            _ => {}
        }
        WalkControl::Continue
    }
}

fn collect_generic_alias_type_diagnostics(
    scopes: &ScopeTree,
    scope: crate::scopes::ScopeId,
    current_binding: &TypeBinding,
    ty: &Type,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match ty {
        Type::Reference {
            location,
            prefix,
            name,
            name_location,
            parameters,
            ..
        } => {
            for parameter in parameters {
                match parameter {
                    TypeParameter::Type(ty) => collect_generic_alias_type_diagnostics(
                        scopes,
                        scope,
                        current_binding,
                        ty,
                        diagnostics,
                    ),
                    TypeParameter::Pack(pack) => collect_generic_alias_pack_diagnostics(
                        scopes,
                        scope,
                        current_binding,
                        pack,
                        diagnostics,
                    ),
                }
            }
            let lookup_name = prefix
                .as_ref()
                .map(|prefix| format!("{}.{}", prefix.as_str(), name.as_str()))
                .unwrap_or_else(|| name.as_str().to_owned());
            if prefix.is_none()
                && current_binding
                    .generic_pack_names
                    .iter()
                    .any(|pack| pack == name.as_str())
            {
                diagnostics.push(generic_pack_used_as_type_diagnostic(
                    name.as_str(),
                    name_location
                        .as_ref()
                        .copied()
                        .map(DiagnosticLocation::from)
                        .unwrap_or_else(DiagnosticLocation::missing),
                ));
                return;
            }
            if prefix.is_none() && binding_owns_generic_name(current_binding, &lookup_name) {
                return;
            }
            let Some((_, binding)) = scopes.lookup_type_with_scope(scope, &lookup_name) else {
                return;
            };
            if !binding.alias_has_generics {
                return;
            }
            let has_parameter_list = type_reference_has_parameter_list(*location, *name_location);
            diagnostics.extend(generic_alias_argument_slot_diagnostics(
                &lookup_name,
                current_binding,
                binding,
                parameters,
            ));
            let reference_location = location
                .as_ref()
                .copied()
                .map(DiagnosticLocation::from)
                .unwrap_or_else(DiagnosticLocation::missing);
            let generic_alias_diagnostic = generic_alias_reference_diagnostic(
                &lookup_name,
                binding,
                parameters,
                has_parameter_list,
                reference_location,
            );
            if let Some(diagnostic) = generic_alias_diagnostic {
                diagnostics.push(diagnostic);
            }
            if recursive_alias_reference_uses_different_parameters(
                current_binding,
                &lookup_name,
                parameters,
            ) {
                diagnostics.push(recursive_restraint_violation_diagnostic(
                    location
                        .as_ref()
                        .copied()
                        .map(DiagnosticLocation::from)
                        .unwrap_or_else(DiagnosticLocation::missing),
                    &current_binding.name,
                ));
            }
        }
        Type::Group { inner, .. } => {
            collect_generic_alias_type_diagnostics(
                scopes,
                scope,
                current_binding,
                inner,
                diagnostics,
            );
        }
        Type::Union { types, .. }
        | Type::Intersection { types, .. }
        | Type::Error { types, .. } => {
            for ty in types {
                collect_generic_alias_type_diagnostics(
                    scopes,
                    scope,
                    current_binding,
                    ty,
                    diagnostics,
                );
            }
        }
        Type::Function {
            arg_types,
            return_types,
            ..
        } => {
            for ty in &arg_types.types {
                collect_generic_alias_type_diagnostics(
                    scopes,
                    scope,
                    current_binding,
                    ty,
                    diagnostics,
                );
            }
            if let Some(tail) = arg_types.tail_type.as_deref() {
                collect_generic_alias_pack_diagnostics(
                    scopes,
                    scope,
                    current_binding,
                    tail,
                    diagnostics,
                );
            }
            collect_generic_alias_pack_diagnostics(
                scopes,
                scope,
                current_binding,
                return_types,
                diagnostics,
            );
        }
        Type::Table { props, indexer, .. } => {
            for prop in props {
                collect_generic_alias_type_diagnostics(
                    scopes,
                    scope,
                    current_binding,
                    &prop.prop_type,
                    diagnostics,
                );
            }
            if let Some(indexer) = indexer {
                collect_generic_alias_type_diagnostics(
                    scopes,
                    scope,
                    current_binding,
                    &indexer.index_type,
                    diagnostics,
                );
                collect_generic_alias_type_diagnostics(
                    scopes,
                    scope,
                    current_binding,
                    &indexer.result_type,
                    diagnostics,
                );
            }
        }
        Type::SingletonString { .. }
        | Type::SingletonBool { .. }
        | Type::Typeof { .. }
        | Type::Optional { .. } => {}
    }
}

fn generic_alias_argument_slot_diagnostics(
    alias_name: &str,
    current_binding: &TypeBinding,
    binding: &TypeBinding,
    parameters: &[TypeParameter],
) -> Vec<TypeDiagnostic> {
    let mut diagnostics = Vec::new();
    // Luau keeps ordinary alias instantiation shape errors aggregate-only; the
    // slot diagnostics surface for malformed recursive self-references.
    if alias_name != current_binding.name {
        return diagnostics;
    }

    for parameter in parameters.iter().take(binding.generic_names.len()) {
        let TypeParameter::Pack(pack) = parameter else {
            continue;
        };
        diagnostics.push(generic_alias_pack_in_type_slot_diagnostic(
            alias_name,
            type_pack_location(pack),
        ));
        if let Some(name) = generic_type_used_as_pack_name(pack)
            && binding_owns_generic_type_name(current_binding, name)
        {
            diagnostics.push(generic_type_used_as_pack_diagnostic(
                name,
                type_pack_location(pack),
            ));
        }
    }
    diagnostics
}

fn type_pack_location(pack: &TypePack) -> DiagnosticLocation {
    DiagnosticLocation::from_opt(pack.location())
}

fn type_location(ty: &Type) -> DiagnosticLocation {
    DiagnosticLocation::from_opt(ty.location())
}

fn generic_type_used_as_pack_name(pack: &TypePack) -> Option<&str> {
    match pack {
        TypePack::Generic { name, .. } => Some(name.as_str()),
        TypePack::Variadic { variadic_type, .. } => {
            let Type::Reference {
                prefix: None,
                name,
                parameters,
                ..
            } = variadic_type.as_ref()
            else {
                return None;
            };
            parameters.is_empty().then_some(name.as_str())
        }
        TypePack::Explicit { .. } => None,
    }
}

fn collect_generic_alias_pack_diagnostics(
    scopes: &ScopeTree,
    scope: crate::scopes::ScopeId,
    current_binding: &TypeBinding,
    pack: &TypePack,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match pack {
        TypePack::Explicit { type_list, .. } => {
            for ty in &type_list.types {
                collect_generic_alias_type_diagnostics(
                    scopes,
                    scope,
                    current_binding,
                    ty,
                    diagnostics,
                );
            }
            if let Some(tail) = type_list.tail_type.as_deref() {
                collect_generic_alias_pack_diagnostics(
                    scopes,
                    scope,
                    current_binding,
                    tail,
                    diagnostics,
                );
            }
        }
        TypePack::Variadic { variadic_type, .. } => {
            collect_generic_alias_type_diagnostics(
                scopes,
                scope,
                current_binding,
                variadic_type,
                diagnostics,
            );
        }
        TypePack::Generic { .. } => {}
    }
}

fn generic_alias_reference_diagnostic(
    alias_name: &str,
    binding: &TypeBinding,
    parameters: &[TypeParameter],
    has_parameter_list: bool,
    location: DiagnosticLocation,
) -> Option<TypeDiagnostic> {
    if arguments_are_out_of_order(parameters, binding.generic_names.len()) {
        return Some(generic_alias_parameter_order_diagnostic(
            alias_name, location,
        ));
    }

    let (actual_types, actual_packs) = argument_counts(parameters, binding.generic_names.len());
    let required_types = binding
        .generic_defaults
        .iter()
        .filter(|default| default.is_none())
        .count();

    let pack_before_required_type = parameters
        .iter()
        .take(binding.generic_names.len())
        .any(|parameter| matches!(parameter, TypeParameter::Pack(_)));
    let missing_required_type = actual_types < required_types || pack_before_required_type;
    let extra_type =
        actual_types > binding.generic_names.len() && binding.generic_pack_names.is_empty();
    let extra_pack = actual_packs > binding.generic_pack_names.len();
    let omitted_parameter_list = parameters.is_empty()
        && !has_parameter_list
        && (required_types > 0 || binding.generic_pack_defaults.iter().any(Option::is_none));

    // A missing required type pack is reported by the lowering path
    // (`report_generic_alias_parameter_count`) at the real reference location;
    // multi-pack aliases must not also report it here at a synthetic location,
    // which would double-count the diagnostic.
    if omitted_parameter_list || missing_required_type || extra_type || extra_pack {
        Some(generic_alias_parameter_count_diagnostic(
            alias_name,
            binding.generic_names.len(),
            binding.generic_pack_names.len(),
            actual_types,
            actual_packs,
            location,
        ))
    } else {
        None
    }
}

fn generic_alias_parameter_count_diagnostic(
    alias_name: &str,
    expected_types: usize,
    expected_packs: usize,
    actual_types: usize,
    actual_packs: usize,
    location: DiagnosticLocation,
) -> TypeDiagnostic {
    TypeDiagnostic::error(DiagnosticCategory::Generic, location).with_typed(
        crate::diagnostic::Payload::GenericAliasParameterCount {
            alias: alias_name.to_owned(),
            expected_type_parameters: expected_types,
            expected_type_pack_parameters: expected_packs,
            actual_type_parameters: actual_types,
            actual_type_pack_parameters: actual_packs,
        },
    )
}

fn generic_alias_parameter_order_diagnostic(
    alias_name: &str,
    location: DiagnosticLocation,
) -> TypeDiagnostic {
    TypeDiagnostic::error(DiagnosticCategory::Generic, location)
        .with_context("Type parameters must come before type pack parameters")
        .with_typed(crate::diagnostic::Payload::GenericAliasParameterOrder {
            alias: alias_name.to_owned(),
        })
}

fn recursive_alias_reference_uses_different_parameters(
    binding: &TypeBinding,
    lookup_name: &str,
    parameters: &[TypeParameter],
) -> bool {
    if lookup_name != binding.name || !binding.alias_has_generics {
        return false;
    }

    let (actual_types, actual_packs) = argument_counts(parameters, binding.generic_names.len());
    if actual_types > binding.generic_names.len()
        || arguments_are_out_of_order(parameters, binding.generic_names.len())
    {
        return true;
    }
    if actual_packs > binding.generic_pack_names.len() {
        return true;
    }

    let mut type_arguments = Vec::new();
    let mut pack_arguments = Vec::new();
    let mut saw_pack = false;
    for (index, parameter) in parameters.iter().enumerate() {
        match parameter {
            TypeParameter::Pack(pack) => {
                saw_pack = true;
                pack_arguments.push(pack);
            }
            TypeParameter::Type(ty)
                if (saw_pack || index >= binding.generic_names.len())
                    && type_argument_can_follow_pack(ty) =>
            {
                return true;
            }
            TypeParameter::Type(ty) => type_arguments.push(ty.as_ref()),
        }
    }

    if binding
        .generic_names
        .iter()
        .zip(&type_arguments)
        .any(|(generic, argument)| !type_argument_is_generic(argument, generic))
    {
        return true;
    }
    if type_arguments.len() < binding.generic_names.len()
        && binding.generic_defaults[type_arguments.len()..]
            .iter()
            .any(Option::is_some)
    {
        return true;
    }

    if binding
        .generic_pack_names
        .iter()
        .zip(&pack_arguments)
        .any(|(generic, argument)| !pack_argument_is_generic(argument, generic))
    {
        return true;
    }
    if pack_arguments.len() < binding.generic_pack_names.len()
        && binding.generic_pack_defaults[pack_arguments.len()..]
            .iter()
            .any(Option::is_some)
    {
        return true;
    }

    false
}

fn type_argument_is_generic(ty: &Type, generic_name: &str) -> bool {
    matches!(
        ty,
        Type::Reference {
            prefix: None,
            name,
            parameters,
            ..
        } if name.as_str() == generic_name && parameters.is_empty()
    )
}

fn binding_owns_generic_type_name(binding: &TypeBinding, name: &str) -> bool {
    binding.generic_names.iter().any(|generic| generic == name)
}

fn pack_argument_is_generic(pack: &TypePack, generic_name: &str) -> bool {
    matches!(
        pack,
        TypePack::Generic { name, .. } if name.as_str() == generic_name
    )
}

fn generic_alias_pack_in_type_slot_diagnostic(
    alias_name: &str,
    location: DiagnosticLocation,
) -> TypeDiagnostic {
    TypeDiagnostic::error(DiagnosticCategory::Generic, location)
        .with_context(format!(
            "Generic type alias '{alias_name}' expects a type argument, but a type pack was supplied"
        ))
        .with_typed(
            crate::diagnostic::Payload::GenericAliasPackInTypeSlot {
                alias: alias_name.to_owned(),
            },
        )
}

pub fn generic_type_used_as_pack_diagnostic(
    name: &str,
    location: DiagnosticLocation,
) -> TypeDiagnostic {
    TypeDiagnostic::error(DiagnosticCategory::Generic, location)
        .with_context(format!(
            "Generic type '{name}' is used as a generic type pack"
        ))
        .with_typed(crate::diagnostic::Payload::GenericTypeUsedAsPack {
            type_parameter: name.to_owned(),
        })
}

/// Builds the diagnostic for a variadic type-pack parameter (`A...`) referenced
/// where a regular type is expected (`A`). Shared with generation-time lowering
/// so both the eager generic-alias-body validation and on-use lowering produce
/// an identical diagnostic, which lets `dedup` collapse the overlap.
pub fn generic_pack_used_as_type_diagnostic(
    name: &str,
    location: DiagnosticLocation,
) -> TypeDiagnostic {
    TypeDiagnostic::error(DiagnosticCategory::Generic, location)
        .with_context(format!(
            "Variadic type parameter '{name}...' is used as a regular generic type; consider \
             changing '{name}...' to '{name}' in the generic argument list"
        ))
        .with_typed(crate::diagnostic::Payload::GenericPackUsedAsType {
            type_pack_parameter: name.to_owned(),
        })
}

fn recursive_restraint_violation_diagnostic(
    location: DiagnosticLocation,
    alias_name: &str,
) -> TypeDiagnostic {
    TypeDiagnostic::error(DiagnosticCategory::Generic, location)
        .with_context("Recursive type being used with different parameters.")
        .with_typed(crate::diagnostic::Payload::RecursiveRestraintViolation {
            alias: alias_name.to_owned(),
        })
}

pub fn recursive_type_alias_diagnostic(
    alias_name: &str,
    location: DiagnosticLocation,
) -> TypeDiagnostic {
    TypeDiagnostic::error(DiagnosticCategory::Generic, location)
        .with_context(format!(
            "Recursive type alias '{alias_name}' cannot be resolved"
        ))
        .with_typed(crate::diagnostic::Payload::RecursiveTypeAlias {
            alias: alias_name.to_owned(),
        })
}

fn duplicate_generic_name_diagnostics(binding: &TypeBinding) -> Vec<TypeDiagnostic> {
    let mut seen = BTreeSet::new();
    binding
        .generic_names
        .iter()
        .zip(binding.generic_locations.iter())
        .chain(
            binding
                .generic_pack_names
                .iter()
                .zip(binding.generic_pack_locations.iter()),
        )
        .filter_map(|(name, location)| {
            if seen.insert(name.as_str()) {
                None
            } else {
                let location = DiagnosticLocation::from_opt(*location);
                Some(
                    TypeDiagnostic::error(DiagnosticCategory::Generic, location)
                        .with_context(format!("Duplicate generic parameter '{name}'"))
                        .with_typed(crate::diagnostic::Payload::DuplicateGenericParameter {
                            alias: binding.name.clone(),
                            parameter: name.clone(),
                        }),
                )
            }
        })
        .collect()
}

fn alias_has_transparent_cycle(
    scopes: &ScopeTree,
    scope: crate::scopes::ScopeId,
    target: &str,
    current_binding: &TypeBinding,
    ty: &Type,
    stack: &mut Vec<String>,
) -> bool {
    alias_has_transparent_cycle_for(scopes, scope, &[target], current_binding, ty, stack)
}

/// Walks an alias body for a transparent occurrence of any of `targets`.
///
/// `targets` is a set rather than a single name so recursion through a generic
/// alias application can substitute: when `Wrapped` appears as the argument in
/// `Table<Wrapped>`, we descend into `Table`'s body with the corresponding
/// generic parameter (`T`) added to the target set. A target in type-*argument*
/// position is therefore not transparent by itself — it is transparent only if
/// the referenced alias uses that parameter transparently (`Table<T> = { a: T }`
/// puts `T` under a table field, so the cycle is well-founded/equirecursive,
/// not transparent). Head cycles (`A = B; B = A`) are still caught because the
/// original target stays in the set while descending through head references.
fn alias_has_transparent_cycle_for(
    scopes: &ScopeTree,
    scope: crate::scopes::ScopeId,
    targets: &[&str],
    current_binding: &TypeBinding,
    ty: &Type,
    stack: &mut Vec<String>,
) -> bool {
    match ty {
        Type::Reference {
            prefix,
            name,
            parameters,
            ..
        } => {
            let lookup_name = prefix
                .as_ref()
                .map(|prefix| format!("{}.{}", prefix.as_str(), name.as_str()))
                .unwrap_or_else(|| name.as_str().to_owned());
            // A reference to a target is a transparent cycle even when the
            // referenced name is a generic parameter of the current alias — that
            // is exactly the substituted-passthrough case (`B<T> = T` reached via
            // `A = B<A>`). So the target check precedes the owns-generic guard.
            if targets.contains(&lookup_name.as_str()) {
                return true;
            }
            if binding_owns_generic_name(current_binding, &lookup_name) {
                return false;
            }
            if stack.iter().any(|name| name == &lookup_name) {
                return false;
            }
            let Some((_, binding)) = scopes.lookup_type_with_scope(scope, &lookup_name) else {
                return false;
            };
            let Some(alias) = binding.alias.as_ref() else {
                return false;
            };
            // Descend into the referenced alias body to follow head cycles
            // (`A = B; B = A`). On top of the original targets — which detect a
            // reference looping back to the source alias head — each argument
            // that mentions a target binds the referenced alias's generic
            // parameter at the same position, so add those parameter names. A
            // target reached only through a type-argument that the referenced
            // alias uses non-transparently (under a table field) contributes no
            // matching reference and so is correctly not a transparent cycle.
            let mut descend_targets: Vec<&str> = targets.to_vec();
            for (index, parameter) in parameters.iter().enumerate() {
                if let TypeParameter::Type(ty) = parameter
                    && alias_has_transparent_cycle_for(
                        scopes,
                        scope,
                        targets,
                        current_binding,
                        ty,
                        stack,
                    )
                    && let Some(generic) = binding.generic_names.get(index)
                {
                    descend_targets.push(generic.as_str());
                }
            }
            stack.push(lookup_name);
            let has_cycle = alias_has_transparent_cycle_for(
                scopes,
                scope,
                &descend_targets,
                binding,
                alias,
                stack,
            );
            if !has_cycle {
                stack.pop();
            }
            has_cycle
        }
        Type::Group { inner, .. } => {
            alias_has_transparent_cycle_for(scopes, scope, targets, current_binding, inner, stack)
        }
        Type::Union { types, .. } | Type::Intersection { types, .. } => types.iter().any(|ty| {
            alias_has_transparent_cycle_for(scopes, scope, targets, current_binding, ty, stack)
        }),
        Type::Error { types, .. } => types.iter().any(|ty| {
            alias_has_transparent_cycle_for(scopes, scope, targets, current_binding, ty, stack)
        }),
        Type::Typeof { .. }
        | Type::Optional { .. }
        | Type::Function { .. }
        | Type::Table { .. }
        | Type::SingletonString { .. }
        | Type::SingletonBool { .. } => false,
    }
}

fn binding_owns_generic_name(binding: &TypeBinding, name: &str) -> bool {
    binding
        .generic_names
        .iter()
        .chain(binding.generic_pack_names.iter())
        .any(|generic| generic == name)
}

pub fn materialize_root_type_aliases(
    scopes: &mut ScopeTree,
    dfg: &DataFlowGraph,
    arena: &mut Arena,
    mode: AnalysisMode,
    global_defs: &BTreeMap<String, TypeId>,
) -> Vec<TypeDiagnostic> {
    let mut diagnostics = Vec::new();
    let root = scopes.root();
    let aliases = scopes
        .get(root)
        .type_bindings
        .iter()
        .filter_map(|(name, binding)| {
            (binding.ty.is_none() && !binding.alias_has_generics)
                .then_some(binding.alias.as_ref())
                .flatten()
                .map(|alias| (name.clone(), alias.clone()))
        })
        .collect::<Vec<_>>();
    for (name, alias) in aliases {
        let (ty, alias_diagnostics) =
            lower_type_annotation_with_globals(&alias, scopes, dfg, arena, mode, global_defs);
        if !alias_diagnostics.is_empty() {
            diagnostics.extend(alias_diagnostics);
            continue;
        }
        match arena.get(ty).clone() {
            TypeKind::Table(mut table) => {
                table.name = Some(name.clone());
                arena.replace(ty, TypeKind::Table(table));
            }
            TypeKind::Metatable {
                table,
                metatable,
                name: _,
            } => {
                arena.replace(
                    ty,
                    TypeKind::Metatable {
                        table,
                        metatable,
                        name: Some(name.clone()),
                    },
                );
            }
            _ => {}
        }
        if let Some(binding) = scopes.get(root).type_bindings.get(&name) {
            let mut binding = binding.clone();
            binding.ty = Some(ty);
            scopes.define_type_binding(root, binding);
        }
    }
    diagnostics
}
