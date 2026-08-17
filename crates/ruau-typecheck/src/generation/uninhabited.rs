//! Diagnostics for type functions that reduce to uninhabited (`never`) types.
//!
//! When a function's parameter or return types embed type-function instances
//! (such as `index<...>` or `keyof<...>`) that are statically uninhabited, the
//! checker reports them once per distinct instance. This module owns that
//! traversal, factored out of the main expression constraint generator.

use std::collections::BTreeSet;

use ruau_syntax::Location;

use crate::{
    diagnostics::{Diagnostic, DiagnosticLocation},
    generation::state::ExpressionConstraintGenerator,
    type_function::{Reduction, SETMETATABLE_TYPE_FUNCTION, TypeFunctionRuntime},
    types::{FunctionType, TypeId, TypeKind, TypePackId, TypePackKind},
};

#[allow(clippy::multiple_inherent_impl)]
impl<'a> ExpressionConstraintGenerator<'a> {
    pub(crate) fn report_uninhabited_type_function_diagnostics_for_function(
        &mut self,
        function: &FunctionType,
        location: Option<Location>,
    ) {
        let mut reported = BTreeSet::new();
        self.report_uninhabited_type_functions_in_function(
            function,
            &BTreeSet::new(),
            &mut reported,
            DiagnosticLocation::from_opt(location),
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        );
    }

    pub(crate) fn report_uninhabited_type_function_diagnostics_for_type(
        &mut self,
        ty: TypeId,
        location: Option<DiagnosticLocation>,
    ) {
        let mut reported = BTreeSet::new();
        self.report_uninhabited_type_functions_in_type(
            ty,
            &BTreeSet::new(),
            &mut reported,
            location.unwrap_or_else(DiagnosticLocation::missing),
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        );
    }

    pub(crate) fn report_uninhabited_type_function_diagnostic(
        &mut self,
        ty: TypeId,
        location: Option<DiagnosticLocation>,
    ) {
        let instance = self.arena.summary(ty);
        self.generated
            .diagnostics
            .push(Diagnostic::uninhabited_type_function(
                instance,
                location.unwrap_or_else(DiagnosticLocation::missing),
            ));
    }

    fn report_uninhabited_type_functions_in_function(
        &mut self,
        function: &FunctionType,
        inherited_bounds: &BTreeSet<TypeId>,
        reported: &mut BTreeSet<String>,
        location: DiagnosticLocation,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) {
        let mut bounds = inherited_bounds.clone();
        let mut bound_seen_types = BTreeSet::new();
        let mut bound_seen_packs = BTreeSet::new();
        self.collect_uninhabited_generic_bounds_in_pack(
            function.arguments,
            &function.generics,
            &mut bounds,
            &mut bound_seen_types,
            &mut bound_seen_packs,
        );
        self.report_uninhabited_type_functions_in_pack(
            function.arguments,
            &bounds,
            reported,
            location,
            seen_types,
            seen_packs,
        );
        self.report_uninhabited_type_functions_in_pack(
            function.returns,
            &bounds,
            reported,
            location,
            seen_types,
            seen_packs,
        );
    }

    fn collect_uninhabited_generic_bounds_in_pack(
        &mut self,
        pack: TypePackId,
        generics: &[crate::types::GenericType],
        bounds: &mut BTreeSet<TypeId>,
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
                    self.collect_uninhabited_generic_bounds_in_type(
                        ty, generics, bounds, seen_types, seen_packs,
                    );
                }
                if let Some(tail) = tail {
                    self.collect_uninhabited_generic_bounds_in_pack(
                        tail, generics, bounds, seen_types, seen_packs,
                    );
                }
            }
            TypePackKind::Variadic { ty } => self.collect_uninhabited_generic_bounds_in_type(
                ty, generics, bounds, seen_types, seen_packs,
            ),
            TypePackKind::Bound(bound) => self.collect_uninhabited_generic_bounds_in_pack(
                bound, generics, bounds, seen_types, seen_packs,
            ),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => {}
        }
    }

    fn collect_uninhabited_generic_bounds_in_type(
        &mut self,
        ty: TypeId,
        generics: &[crate::types::GenericType],
        bounds: &mut BTreeSet<TypeId>,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return;
        }
        match self.arena.get(ty).clone() {
            TypeKind::Intersection(options)
                if options
                    .iter()
                    .any(|option| self.type_is_uninhabited_after_bounds(*option, bounds)) =>
            {
                for option in options {
                    let option = self.arena.follow(option);
                    if self.owned_generic_type_id(option, generics).is_some() {
                        bounds.insert(option);
                    }
                    self.collect_uninhabited_generic_bounds_in_type(
                        option, generics, bounds, seen_types, seen_packs,
                    );
                }
            }
            TypeKind::Function(function) => {
                self.collect_uninhabited_generic_bounds_in_pack(
                    function.arguments,
                    generics,
                    bounds,
                    seen_types,
                    seen_packs,
                );
                self.collect_uninhabited_generic_bounds_in_pack(
                    function.returns,
                    generics,
                    bounds,
                    seen_types,
                    seen_packs,
                );
            }
            TypeKind::Table(table) => {
                for ty in table.instantiated_type_params {
                    self.collect_uninhabited_generic_bounds_in_type(
                        ty, generics, bounds, seen_types, seen_packs,
                    );
                }
                for property in table.properties.values() {
                    self.collect_uninhabited_generic_bounds_in_type(
                        property.ty,
                        generics,
                        bounds,
                        seen_types,
                        seen_packs,
                    );
                }
                if let Some(indexer) = table.indexer {
                    self.collect_uninhabited_generic_bounds_in_type(
                        indexer.key,
                        generics,
                        bounds,
                        seen_types,
                        seen_packs,
                    );
                    self.collect_uninhabited_generic_bounds_in_type(
                        indexer.value,
                        generics,
                        bounds,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Extern { properties, .. } => {
                for property in properties.values() {
                    self.collect_uninhabited_generic_bounds_in_type(
                        property.ty,
                        generics,
                        bounds,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.collect_uninhabited_generic_bounds_in_type(
                    table, generics, bounds, seen_types, seen_packs,
                );
                self.collect_uninhabited_generic_bounds_in_type(
                    metatable, generics, bounds, seen_types, seen_packs,
                );
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => {
                for ty in arguments {
                    self.collect_uninhabited_generic_bounds_in_type(
                        ty, generics, bounds, seen_types, seen_packs,
                    );
                }
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => self
                .collect_uninhabited_generic_bounds_in_type(
                    inner, generics, bounds, seen_types, seen_packs,
                ),
            TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => {}
        }
    }

    fn report_uninhabited_type_functions_in_pack(
        &mut self,
        pack: TypePackId,
        bounds: &BTreeSet<TypeId>,
        reported: &mut BTreeSet<String>,
        location: DiagnosticLocation,
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
                    self.report_uninhabited_type_functions_in_type(
                        ty, bounds, reported, location, seen_types, seen_packs,
                    );
                }
                if let Some(tail) = tail {
                    self.report_uninhabited_type_functions_in_pack(
                        tail, bounds, reported, location, seen_types, seen_packs,
                    );
                }
            }
            TypePackKind::Variadic { ty } => self.report_uninhabited_type_functions_in_type(
                ty, bounds, reported, location, seen_types, seen_packs,
            ),
            TypePackKind::Bound(bound) => self.report_uninhabited_type_functions_in_pack(
                bound, bounds, reported, location, seen_types, seen_packs,
            ),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => {}
        }
    }

    #[allow(clippy::only_used_in_recursion)]
    fn report_uninhabited_type_functions_in_type(
        &mut self,
        ty: TypeId,
        bounds: &BTreeSet<TypeId>,
        reported: &mut BTreeSet<String>,
        location: DiagnosticLocation,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return;
        }
        match self.arena.get(ty).clone() {
            TypeKind::TypeFunctionInstance { name, arguments } => {
                if name == "index" && self.index_has_direct_uninhabited_operand(&arguments, bounds)
                {
                    self.report_uninhabited_type_function_instance(ty, reported, location);
                    return;
                }
                if name == SETMETATABLE_TYPE_FUNCTION
                    && self.type_function_reduces_to_never(&name, &arguments)
                {
                    self.report_uninhabited_type_function_instance(ty, reported, location);
                    return;
                }
                if name == "keyof"
                    && arguments.first().is_some_and(|target| {
                        self.type_is_uninhabited_after_bounds(*target, bounds)
                    })
                {
                    return;
                }
                for argument in arguments {
                    self.report_uninhabited_type_functions_in_type(
                        argument, bounds, reported, location, seen_types, seen_packs,
                    );
                }
            }
            TypeKind::Function(function) => self.report_uninhabited_type_functions_in_function(
                &function, bounds, reported, location, seen_types, seen_packs,
            ),
            TypeKind::Table(table) => {
                for ty in table.instantiated_type_params {
                    self.report_uninhabited_type_functions_in_type(
                        ty, bounds, reported, location, seen_types, seen_packs,
                    );
                }
                for property in table.properties.values() {
                    self.report_uninhabited_type_functions_in_type(
                        property.ty,
                        bounds,
                        reported,
                        location,
                        seen_types,
                        seen_packs,
                    );
                }
                if let Some(indexer) = table.indexer {
                    self.report_uninhabited_type_functions_in_type(
                        indexer.key,
                        bounds,
                        reported,
                        location,
                        seen_types,
                        seen_packs,
                    );
                    self.report_uninhabited_type_functions_in_type(
                        indexer.value,
                        bounds,
                        reported,
                        location,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Extern { properties, .. } => {
                for property in properties.values() {
                    self.report_uninhabited_type_functions_in_type(
                        property.ty,
                        bounds,
                        reported,
                        location,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.report_uninhabited_type_functions_in_type(
                    table, bounds, reported, location, seen_types, seen_packs,
                );
                self.report_uninhabited_type_functions_in_type(
                    metatable, bounds, reported, location, seen_types, seen_packs,
                );
            }
            TypeKind::Union(types) | TypeKind::Intersection(types) => {
                for ty in types {
                    self.report_uninhabited_type_functions_in_type(
                        ty, bounds, reported, location, seen_types, seen_packs,
                    );
                }
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => self
                .report_uninhabited_type_functions_in_type(
                    inner, bounds, reported, location, seen_types, seen_packs,
                ),
            TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => {}
        }
    }

    fn owned_generic_type_id(
        &self,
        ty: TypeId,
        generics: &[crate::types::GenericType],
    ) -> Option<TypeId> {
        let TypeKind::Generic(generic) = self.arena.get(self.arena.follow(ty)) else {
            return None;
        };
        generics.iter().any(|owned| owned == generic).then_some(ty)
    }

    fn type_is_uninhabited_after_bounds(&mut self, ty: TypeId, bounds: &BTreeSet<TypeId>) -> bool {
        self.type_is_uninhabited_after_bounds_with(ty, bounds, &mut BTreeSet::new())
    }

    fn type_is_uninhabited_after_bounds_with(
        &mut self,
        ty: TypeId,
        bounds: &BTreeSet<TypeId>,
        seen: &mut BTreeSet<TypeId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if bounds.contains(&ty) || matches!(self.arena.get(ty), TypeKind::Never) {
            return true;
        }
        if !seen.insert(ty) {
            return false;
        }
        match self.arena.get(ty).clone() {
            TypeKind::TypeFunctionInstance { name, arguments } => {
                if name == "keyof" {
                    return arguments.first().is_some_and(|target| {
                        self.type_is_uninhabited_after_bounds_with(*target, bounds, seen)
                    }) || self.type_function_reduces_to_never(&name, &arguments);
                }
                if name == "index"
                    && arguments.iter().any(|argument| {
                        self.type_is_uninhabited_after_bounds_with(*argument, bounds, seen)
                    })
                {
                    return true;
                }
                self.type_function_reduces_to_never(&name, &arguments)
            }
            TypeKind::Intersection(types) => types
                .iter()
                .any(|ty| self.type_is_uninhabited_after_bounds_with(*ty, bounds, seen)),
            TypeKind::Union(types) => {
                !types.is_empty()
                    && types
                        .iter()
                        .all(|ty| self.type_is_uninhabited_after_bounds_with(*ty, bounds, seen))
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.type_is_uninhabited_after_bounds_with(inner, bounds, seen)
            }
            _ => false,
        }
    }

    fn type_function_reduces_to_never(&mut self, name: &str, arguments: &[TypeId]) -> bool {
        let checkpoint = self.arena.checkpoint();
        let reduction = TypeFunctionRuntime::new().reduce_allocating(self.arena, name, arguments);
        let reduces_to_never = matches!(
            reduction,
            Reduction::Reduced(reduced)
                if matches!(self.arena.get(self.arena.follow(reduced)), TypeKind::Never)
        );
        self.arena.rollback_to(checkpoint);
        reduces_to_never
    }

    fn report_uninhabited_type_function_instance(
        &mut self,
        ty: TypeId,
        reported: &mut BTreeSet<String>,
        location: DiagnosticLocation,
    ) {
        let instance = self.arena.summary(ty);
        if reported.insert(instance.clone()) {
            self.generated
                .diagnostics
                .push(Diagnostic::uninhabited_type_function(instance, location));
        }
    }

    fn index_has_direct_uninhabited_operand(
        &mut self,
        arguments: &[TypeId],
        bounds: &BTreeSet<TypeId>,
    ) -> bool {
        let [base, key] = arguments else {
            return false;
        };
        self.direct_index_operand_is_uninhabited(*base, bounds)
            || self.direct_index_operand_is_uninhabited(*key, bounds)
    }

    fn direct_index_operand_is_uninhabited(
        &mut self,
        ty: TypeId,
        bounds: &BTreeSet<TypeId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if bounds.contains(&ty) || matches!(self.arena.get(ty), TypeKind::Never) {
            return true;
        }
        match self.arena.get(ty).clone() {
            TypeKind::Intersection(types) => types
                .iter()
                .any(|ty| self.type_is_uninhabited_after_bounds(*ty, bounds)),
            TypeKind::TypeFunctionInstance { .. } => false,
            _ => false,
        }
    }
}
