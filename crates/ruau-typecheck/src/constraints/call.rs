use std::collections::BTreeSet;

use super::{CallConstraintContext, ConstraintSolveError, ConstraintSolver};
use crate::{
    call_pack::CallParameterPack,
    diagnostics::DiagnosticLocation,
    member_access,
    overload::{OverloadError, failed_overload_return_pack, resolve_call_for_constraint},
    subtype::{SubtypeError, Subtyper},
    type_function::{Reduction, TypeFunctionRuntime},
    types::{
        Arena, PrimitiveType, SingletonType, TypeId, TypeKind, TypePackId, TypePackKind,
        TypePathComponent,
    },
};

fn pack_contains_variadic(
    arena: &Arena,
    pack: TypePackId,
    seen: &mut BTreeSet<TypePackId>,
) -> bool {
    let pack = arena.follow_pack(pack);
    if !seen.insert(pack) {
        return false;
    }
    match arena.get_pack(pack) {
        TypePackKind::Variadic { .. } => true,
        TypePackKind::List { tail, .. } => {
            tail.is_some_and(|tail| pack_contains_variadic(arena, tail, seen))
        }
        TypePackKind::Bound(tail) => pack_contains_variadic(arena, *tail, seen),
        TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
    }
}
fn overload_single_argument_subtype_error(
    arena: &Arena,
    error: &OverloadError,
) -> Option<(usize, SubtypeError)> {
    let OverloadError::NoMatch { rejected, .. } = error else {
        return None;
    };
    let [(candidate, subtype_error)] = rejected.as_slice() else {
        return None;
    };
    let TypeKind::Function(function) = arena.get(arena.follow(*candidate)) else {
        return None;
    };
    if !pack_contains_variadic(arena, function.arguments, &mut BTreeSet::new()) {
        return None;
    }
    let argument_index = subtype_error
        .path
        .components()
        .iter()
        .find_map(|component| {
            if let TypePathComponent::Index { index } = component {
                Some(*index)
            } else {
                None
            }
        })?;
    Some((argument_index, subtype_error.clone()))
}
fn is_deferred_operator_type_function(name: &str) -> bool {
    matches!(
        name,
        "sub" | "mul" | "div" | "idiv" | "mod" | "pow" | "unm" | "concat" | "len"
    )
}
impl<'a> ConstraintSolver<'a> {
    pub(super) fn solve_call(
        &mut self,
        callee: TypeId,
        arguments: TypePackId,
        context: CallConstraintContext,
        location: Option<DiagnosticLocation>,
    ) -> Result<(), ConstraintSolveError> {
        let CallConstraintContext {
            nonstrict_checked_arguments,
            argument_locations,
            expected_returns,
            from_call_expression,
        } = context;

        if member_access::type_is_dynamic(self.arena, callee) {
            if let Some(expected_returns) = expected_returns {
                self.bind_dynamic_call_returns(expected_returns);
            }
            return Ok(());
        }
        // A free callee can be instantiated to a function with the source call's
        // argument and return packs. Avoid resolving it as an empty-return
        // synthetic function; surrounding expected return constraints will
        // shape the result values.
        if matches!(self.arena.get(self.arena.follow(callee)), TypeKind::Free(_)) {
            return Ok(());
        }

        let callee = self.instantiate_call_target(callee);
        let direct_signature = match self.arena.get(self.arena.follow(callee)).clone() {
            TypeKind::Function(function) => Some(function),
            _ => None,
        };
        if let (Some(signature), Some(expected_returns)) = (&direct_signature, expected_returns)
            && self.return_pack_can_use_expected_guidance(signature.returns)
            && self.expected_return_pack_can_guide(expected_returns)
        {
            drop(self.infer_call_returns(signature.returns, expected_returns));
        }

        let mut resolution = match resolve_call_for_constraint(
            self.arena,
            callee,
            arguments,
            expected_returns.is_none(),
            nonstrict_checked_arguments,
            from_call_expression,
        ) {
            Ok(resolution) => resolution,
            Err(error) => {
                if let Some(expected_returns) = expected_returns
                    && let Some(union_returns) = self.ambiguous_union_call_returns(callee, &error)
                {
                    self.require_pack_subtype(union_returns, expected_returns)
                        .map_err(|error| error.with_location(location))?;
                    self.infer_call_returns(union_returns, expected_returns)
                        .map_err(|error| error.with_default_location(location))?;
                    return Ok(());
                }
                if let Some(expected_returns) = expected_returns {
                    if let Some(signature) = &direct_signature {
                        if let Some(parameters) =
                            CallParameterPack::from_list(self.arena, signature.arguments)
                        {
                            self.infer_call_argument_bindings(arguments, parameters);
                        }
                        drop(self.infer_call_returns(signature.returns, expected_returns));
                    } else if let Some(failed_returns) =
                        failed_overload_return_pack(self.arena, &error)
                    {
                        drop(self.infer_call_returns(failed_returns, expected_returns));
                    } else {
                        self.bind_error_call_returns(expected_returns);
                    }
                }
                if let Some((argument_index, subtype_error)) =
                    overload_single_argument_subtype_error(self.arena, &error)
                {
                    let location = argument_locations
                        .get(argument_index)
                        .copied()
                        .flatten()
                        .or(location);
                    return Err(ConstraintSolveError::Subtype(subtype_error)
                        .with_aggregated_location(location));
                }
                return Err(ConstraintSolveError::Overload(error)
                    .with_aggregate_location(location, nonstrict_checked_arguments));
            }
        };

        // The resolved callable may live one level deep inside a metatable's
        // `__call` slot; in that case the outer instantiation pass left its
        // generics untouched. Reinstantiate the resolved signature so each call
        // site gets a fresh binding.
        if !resolution.signature.generics.is_empty()
            || !resolution.signature.generic_packs.is_empty()
        {
            let instantiated = self.instantiate_call_target(resolution.function);
            if instantiated != resolution.function
                && let TypeKind::Function(function) =
                    self.arena.get(self.arena.follow(instantiated)).clone()
            {
                resolution.signature = function.clone();
                resolution.returns = function.returns;
                resolution.function = instantiated;
            }
        }
        if let Some(parameters) =
            CallParameterPack::from_list(self.arena, resolution.signature.arguments)
        {
            let parameters = parameters.for_explicit_arguments(resolution.receiver);
            if resolution.bind_free_arguments_to_selected_parameters {
                self.infer_selected_overload_argument_bindings(arguments, parameters);
            } else {
                self.infer_call_argument_bindings(arguments, parameters);
            }
        }
        if let Some(instance) = self.uninhabited_type_function_in_pack(resolution.returns) {
            return Err(
                ConstraintSolveError::UninhabitedTypeFunction { instance }.with_location(location)
            );
        }
        if let Some(expected_returns) = expected_returns {
            self.require_return_pack_subtype(resolution.returns, expected_returns)
                .map_err(|error| error.with_location(location))?;
            self.infer_call_returns(resolution.returns, expected_returns)
                .map_err(|error| error.with_default_location(location))?;
        }
        Ok(())
    }

    /// Instantiates a generic call target so the function's quantified
    /// type and pack parameters become fresh free variables that the
    /// argument-pack subtype check can constrain. Non-generic
    /// callees, dynamic types, and non-function callees pass through
    /// unchanged.
    fn instantiate_call_target(&mut self, callee: TypeId) -> TypeId {
        let followed = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(followed).clone() else {
            return callee;
        };
        if function.generics.is_empty() && function.generic_packs.is_empty() {
            // Generalize a function whose own free type variables are otherwise
            // unconstrained, so each call site instantiates them afresh instead
            // of sharing (and pinning) one free var. This must consider frees in
            // the *arguments* as well as the returns: `function f(x) return {5}
            // end` is polymorphic in `x` even though its return is concrete, and
            // leaving `x` a shared free leaks the first call's argument type into
            // every later use (`quantification_sharing_types`). A parameter the
            // body pins below a concrete scalar (`x + y` ⇒ `x <: number`) is *not*
            // freely polymorphic, so the argument-driven path skips it. Deferred
            // operator type functions stay blocked unless their runtime knows how
            // to report invalid concrete instantiations at the call site.
            if (self.pack_has_unbound_free(
                function.returns,
                &mut BTreeSet::new(),
                &mut BTreeSet::new(),
            ) || (self.pack_has_unbound_free(
                function.arguments,
                &mut BTreeSet::new(),
                &mut BTreeSet::new(),
            ) && !self
                .pack_has_scalar_constrained_free(function.arguments, &mut BTreeSet::new())))
                && !self.pack_has_open_tail(function.arguments, &mut BTreeSet::new())
                && !self.function_contains_type(&function, followed)
                && !self.type_contains_deferred_operator_type_function(
                    followed,
                    &mut BTreeSet::new(),
                    &mut BTreeSet::new(),
                )
            {
                let generalized =
                    crate::generalize::generalize_function_frees(self.arena, followed);
                if let TypeKind::Function(generalized_function) =
                    self.arena.get(self.arena.follow(generalized)).clone()
                    && (!generalized_function.generics.is_empty()
                        || !generalized_function.generic_packs.is_empty())
                {
                    return self.instantiate_call_target(generalized);
                }
            }
            return callee;
        }
        // A single `Instantiator` shares its (generic-name → fresh-var)
        // map across the function's argument and return packs, so a
        // generic that appears in both positions (e.g. `<T>(T) -> T`)
        // binds to the same fresh variable on both sides.
        let level = crate::types::TypeLevel(0);
        let mut instantiator = crate::generalize::Instantiator::new(self.arena, level);
        let instantiated_args = instantiator.instantiate_pack(function.arguments);
        let instantiated_returns = instantiator.instantiate_pack(function.returns);
        self.arena
            .alloc(TypeKind::Function(crate::types::FunctionType {
                generics: Vec::new(),
                generic_packs: Vec::new(),
                argument_names: function.argument_names,
                has_self: function.has_self,
                is_checked: function.is_checked,
                arguments: instantiated_args,
                returns: instantiated_returns,
            }))
    }
    fn pack_has_open_tail(&self, pack: TypePackId, seen: &mut BTreeSet<TypePackId>) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack).clone() {
            TypePackKind::List { tail, .. } => tail.is_some_and(|tail| {
                self.pack_has_open_tail(tail, seen)
                    || matches!(
                        self.arena.get_pack(self.arena.follow_pack(tail)),
                        TypePackKind::Free { .. } | TypePackKind::Variadic { .. }
                    )
            }),
            TypePackKind::Bound(bound) => self.pack_has_open_tail(bound, seen),
            TypePackKind::Free { .. } | TypePackKind::Variadic { .. } => true,
            TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }
    /// Whether `pack` reaches a free type variable that a constraint pins below
    /// a concrete scalar (recorded in `scalar_constrained_frees`). Used to keep
    /// such parameters shared rather than generalizing them per call.
    fn pack_has_scalar_constrained_free(
        &self,
        pack: TypePackId,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack).clone() {
            TypePackKind::List { types, tail } => {
                types.iter().any(|ty| {
                    self.scalar_constrained_frees
                        .contains(&self.arena.follow(*ty))
                }) || tail
                    .is_some_and(|tail| self.pack_has_scalar_constrained_free(tail, seen_packs))
            }
            TypePackKind::Variadic { ty } => self
                .scalar_constrained_frees
                .contains(&self.arena.follow(ty)),
            TypePackKind::Bound(bound) => self.pack_has_scalar_constrained_free(bound, seen_packs),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }
    fn pack_has_unbound_free(
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
                    .any(|ty| self.type_has_unbound_free(*ty, seen_types, seen_packs))
                    || tail.is_some_and(|tail| {
                        self.pack_has_unbound_free(tail, seen_types, seen_packs)
                    })
            }
            TypePackKind::Variadic { ty } => self.type_has_unbound_free(ty, seen_types, seen_packs),
            TypePackKind::Bound(bound) => self.pack_has_unbound_free(bound, seen_types, seen_packs),
            TypePackKind::Free { .. } => true,
            TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }
    fn function_contains_type(
        &self,
        function: &crate::types::FunctionType,
        needle: TypeId,
    ) -> bool {
        self.pack_contains_type(
            function.arguments,
            needle,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        ) || self.pack_contains_type(
            function.returns,
            needle,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        )
    }
    fn pack_contains_type(
        &self,
        pack: TypePackId,
        needle: TypeId,
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
                    .any(|ty| self.type_contains_type(*ty, needle, seen_types, seen_packs))
                    || tail.is_some_and(|tail| {
                        self.pack_contains_type(tail, needle, seen_types, seen_packs)
                    })
            }
            TypePackKind::Variadic { ty } => {
                self.type_contains_type(ty, needle, seen_types, seen_packs)
            }
            TypePackKind::Bound(bound) => {
                self.pack_contains_type(bound, needle, seen_types, seen_packs)
            }
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }
    fn type_contains_type(
        &self,
        haystack: TypeId,
        needle: TypeId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let haystack = self.arena.follow(haystack);
        let needle = self.arena.follow(needle);
        if haystack == needle {
            return true;
        }
        if !seen_types.insert(haystack) {
            return false;
        }
        match self.arena.get(haystack).clone() {
            TypeKind::Function(function) => {
                self.pack_contains_type(function.arguments, needle, seen_types, seen_packs)
                    || self.pack_contains_type(function.returns, needle, seen_types, seen_packs)
            }
            TypeKind::Table(table) => {
                table
                    .instantiated_type_params
                    .iter()
                    .any(|ty| self.type_contains_type(*ty, needle, seen_types, seen_packs))
                    || table.properties.values().any(|property| {
                        self.type_contains_type(property.ty, needle, seen_types, seen_packs)
                    })
                    || table.indexer.is_some_and(|indexer| {
                        self.type_contains_type(indexer.key, needle, seen_types, seen_packs)
                            || self.type_contains_type(
                                indexer.value,
                                needle,
                                seen_types,
                                seen_packs,
                            )
                    })
            }
            TypeKind::Extern { properties, .. } => properties.values().any(|property| {
                self.type_contains_type(property.ty, needle, seen_types, seen_packs)
            }),
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_contains_type(table, needle, seen_types, seen_packs)
                    || self.type_contains_type(metatable, needle, seen_types, seen_packs)
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => arguments
                .iter()
                .any(|argument| self.type_contains_type(*argument, needle, seen_types, seen_packs)),
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.type_contains_type(inner, needle, seen_types, seen_packs)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }
    fn type_contains_deferred_operator_type_function(
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
            TypeKind::Function(function) => {
                self.pack_contains_deferred_operator_type_function(
                    function.arguments,
                    seen_types,
                    seen_packs,
                ) || self.pack_contains_deferred_operator_type_function(
                    function.returns,
                    seen_types,
                    seen_packs,
                )
            }
            TypeKind::Table(table) => {
                table.instantiated_type_params.iter().any(|ty| {
                    self.type_contains_deferred_operator_type_function(*ty, seen_types, seen_packs)
                }) || table.properties.values().any(|property| {
                    self.type_contains_deferred_operator_type_function(
                        property.ty,
                        seen_types,
                        seen_packs,
                    )
                }) || table.indexer.is_some_and(|indexer| {
                    self.type_contains_deferred_operator_type_function(
                        indexer.key,
                        seen_types,
                        seen_packs,
                    ) || self.type_contains_deferred_operator_type_function(
                        indexer.value,
                        seen_types,
                        seen_packs,
                    )
                })
            }
            TypeKind::Extern { properties, .. } => properties.values().any(|property| {
                self.type_contains_deferred_operator_type_function(
                    property.ty,
                    seen_types,
                    seen_packs,
                )
            }),
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_contains_deferred_operator_type_function(table, seen_types, seen_packs)
                    || self.type_contains_deferred_operator_type_function(
                        metatable, seen_types, seen_packs,
                    )
            }
            TypeKind::TypeFunctionInstance { name, arguments } => {
                is_deferred_operator_type_function(&name)
                    || arguments.iter().any(|argument| {
                        self.type_contains_deferred_operator_type_function(
                            *argument, seen_types, seen_packs,
                        )
                    })
            }
            TypeKind::Union(arguments) | TypeKind::Intersection(arguments) => {
                arguments.iter().any(|argument| {
                    self.type_contains_deferred_operator_type_function(
                        *argument, seen_types, seen_packs,
                    )
                })
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.type_contains_deferred_operator_type_function(inner, seen_types, seen_packs)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }
    fn pack_contains_deferred_operator_type_function(
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
                types.iter().any(|ty| {
                    self.type_contains_deferred_operator_type_function(*ty, seen_types, seen_packs)
                }) || tail.is_some_and(|tail| {
                    self.pack_contains_deferred_operator_type_function(tail, seen_types, seen_packs)
                })
            }
            TypePackKind::Variadic { ty } => {
                self.type_contains_deferred_operator_type_function(ty, seen_types, seen_packs)
            }
            TypePackKind::Bound(bound) => {
                self.pack_contains_deferred_operator_type_function(bound, seen_types, seen_packs)
            }
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }
    fn type_has_unbound_free(
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
                self.pack_has_unbound_free(function.arguments, seen_types, seen_packs)
                    || self.pack_has_unbound_free(function.returns, seen_types, seen_packs)
            }
            TypeKind::Table(table) => {
                table
                    .instantiated_type_params
                    .iter()
                    .any(|ty| self.type_has_unbound_free(*ty, seen_types, seen_packs))
                    || table.properties.values().any(|property| {
                        self.type_has_unbound_free(property.ty, seen_types, seen_packs)
                    })
                    || table.indexer.is_some_and(|indexer| {
                        self.type_has_unbound_free(indexer.key, seen_types, seen_packs)
                            || self.type_has_unbound_free(indexer.value, seen_types, seen_packs)
                    })
            }
            TypeKind::Extern { properties, .. } => properties
                .values()
                .any(|property| self.type_has_unbound_free(property.ty, seen_types, seen_packs)),
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_has_unbound_free(table, seen_types, seen_packs)
                    || self.type_has_unbound_free(metatable, seen_types, seen_packs)
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => arguments
                .iter()
                .any(|argument| self.type_has_unbound_free(*argument, seen_types, seen_packs)),
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.type_has_unbound_free(inner, seen_types, seen_packs)
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
    fn bind_dynamic_call_returns(&mut self, expected_returns: TypePackId) {
        let expected_returns = self.arena.follow_pack(expected_returns);
        let any = self.arena.primitives().any;
        if let TypePackKind::List { types, .. } = self.arena.get_pack(expected_returns).clone() {
            for ty in types {
                let ty = self.arena.follow(ty);
                if matches!(self.arena.get(ty), TypeKind::Free(_)) {
                    self.arena.replace(ty, TypeKind::Bound(any));
                }
            }
        }
    }
    fn bind_error_call_returns(&mut self, expected_returns: TypePackId) {
        let expected_returns = self.arena.follow_pack(expected_returns);
        let error = self.arena.primitives().error;
        if let TypePackKind::List { types, .. } = self.arena.get_pack(expected_returns).clone() {
            for ty in types {
                let ty = self.arena.follow(ty);
                if matches!(self.arena.get(ty), TypeKind::Free(_)) {
                    self.arena.replace(ty, TypeKind::Bound(error));
                }
            }
        }
    }
    fn infer_call_argument_bindings(
        &mut self,
        arguments: TypePackId,
        parameters: CallParameterPack,
    ) {
        self.infer_call_argument_bindings_with(
            arguments,
            parameters,
            false,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        );
    }
    fn infer_selected_overload_argument_bindings(
        &mut self,
        arguments: TypePackId,
        parameters: CallParameterPack,
    ) {
        self.infer_call_argument_bindings_with(
            arguments,
            parameters,
            true,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        );
    }
    fn infer_call_argument_bindings_with(
        &mut self,
        arguments: TypePackId,
        parameters: CallParameterPack,
        bind_free_arguments: bool,
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
        seen_packs: &mut BTreeSet<(TypePackId, TypePackId)>,
    ) {
        let arguments = self.arena.follow_pack(arguments);
        let TypePackKind::List {
            types: argument_types,
            tail: argument_tail,
        } = self.arena.get_pack(arguments).clone()
        else {
            return;
        };

        self.infer_call_argument_tail_binding(
            &argument_types,
            argument_tail,
            parameters.types.len(),
            parameters.tail,
        );

        for (argument, parameter) in argument_types.into_iter().zip(parameters.types) {
            if bind_free_arguments {
                self.infer_selected_overload_argument_type(
                    argument, parameter, seen_types, seen_packs,
                );
            } else {
                self.infer_call_argument_type(argument, parameter, seen_types, seen_packs);
            }
        }
    }
    fn infer_selected_overload_argument_type(
        &mut self,
        argument: TypeId,
        parameter: TypeId,
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
        seen_packs: &mut BTreeSet<(TypePackId, TypePackId)>,
    ) {
        let argument = self.arena.follow(argument);
        let parameter = self.arena.follow(parameter);
        if matches!(self.arena.get(argument), TypeKind::Free(_))
            && !matches!(self.arena.get(parameter), TypeKind::Free(_))
        {
            drop(self.unifier().constrain_subtype(argument, parameter));
            return;
        }
        self.infer_call_argument_type(argument, parameter, seen_types, seen_packs);
    }
    fn infer_call_argument_tail_binding(
        &mut self,
        argument_types: &[TypeId],
        argument_tail: Option<TypePackId>,
        fixed_parameter_count: usize,
        parameter_tail: Option<TypePackId>,
    ) {
        let Some(parameter_tail) = parameter_tail else {
            return;
        };
        let extra_arguments = if argument_types.len() >= fixed_parameter_count {
            &argument_types[fixed_parameter_count..]
        } else {
            &[]
        };
        if extra_arguments.is_empty() && argument_tail.is_none() {
            return;
        }
        let actual_tail = self.arena.alloc_pack(TypePackKind::List {
            types: extra_arguments.to_vec(),
            tail: argument_tail,
        });
        drop(
            self.unifier()
                .constrain_pack_subtype(actual_tail, parameter_tail),
        );
    }
    fn infer_call_returns(
        &mut self,
        returns: TypePackId,
        expected_returns: TypePackId,
    ) -> Result<(), ConstraintSolveError> {
        let returns = self.arena.follow_pack(returns);
        let expected_returns = self.arena.follow_pack(expected_returns);
        match (
            self.arena.get_pack(returns).clone(),
            self.arena.get_pack(expected_returns).clone(),
        ) {
            (TypePackKind::Free { .. }, _) | (_, TypePackKind::Free { .. }) => self
                .unifier()
                .unify_pack(returns, expected_returns)
                .map_err(ConstraintSolveError::Unify),
            (
                TypePackKind::List {
                    types: return_types,
                    tail: return_tail,
                },
                TypePackKind::List {
                    types: expected_types,
                    tail: expected_tail,
                },
            ) => {
                let return_pack =
                    self.arena
                        .flatten_list_pack_from_parts(returns, return_types, return_tail);
                let expected_pack = self.arena.flatten_list_pack_from_parts(
                    expected_returns,
                    expected_types,
                    expected_tail,
                );
                for (actual, expected) in return_pack.types.into_iter().zip(expected_pack.types) {
                    self.infer_call_return_type(actual, expected)?;
                }
                Ok(())
            }
            _ => self
                .unifier()
                .constrain_pack_subtype(returns, expected_returns)
                .map_err(ConstraintSolveError::Unify),
        }
    }
    fn return_pack_can_use_expected_guidance(&self, returns: TypePackId) -> bool {
        let returns = self.arena.normalize_pack(returns);
        returns
            .types
            .iter()
            .any(|ty| !self.type_cannot_use_expected_guidance(*ty))
    }
    fn expected_return_pack_can_guide(&self, expected_returns: TypePackId) -> bool {
        let expected_returns = self.arena.normalize_pack(expected_returns);
        expected_returns.types.iter().any(|ty| {
            !self.type_is_or_contains_free(*ty) && !member_access::type_is_dynamic(self.arena, *ty)
        })
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
    fn infer_call_return_type(
        &mut self,
        actual: TypeId,
        expected: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        self.infer_call_return_guidance(actual, expected);
        let actual = self.arena.follow(actual);
        let expected = self.arena.follow(expected);
        if actual == expected {
            return Ok(());
        }
        if self.type_is_unconstrained_free(actual)
            && matches!(self.arena.get(expected), TypeKind::Free(_))
        {
            let unknown = self.arena.primitives().unknown;
            self.arena.replace(expected, TypeKind::Bound(unknown));
            return Ok(());
        }
        if matches!(self.arena.get(expected), TypeKind::Free(_))
            && let Some(seed) = self.union_without_recursive_seed(actual, expected)
        {
            return self
                .unifier()
                .unify(expected, seed)
                .map_err(ConstraintSolveError::Unify);
        }
        if matches!(self.arena.get(actual), TypeKind::Free(_))
            || matches!(self.arena.get(expected), TypeKind::Free(_))
        {
            return self
                .unifier()
                .unify(actual, expected)
                .map_err(ConstraintSolveError::Unify);
        }
        self.merge_unsealed_table_assignment(actual, expected);
        self.unifier()
            .constrain_subtype(actual, expected)
            .map_err(ConstraintSolveError::Unify)
    }
    fn infer_call_return_guidance(&mut self, actual: TypeId, expected: TypeId) {
        self.infer_call_return_guidance_with(actual, expected, &mut BTreeSet::new());
    }
    fn infer_call_return_guidance_with(
        &mut self,
        actual: TypeId,
        expected: TypeId,
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
    ) {
        let actual = self.arena.follow(actual);
        let expected = self.arena.follow(expected);
        if actual == expected {
            return;
        }
        if !seen_types.insert((actual, expected)) {
            return;
        }
        match (
            self.arena.get(actual).clone(),
            self.arena.get(expected).clone(),
        ) {
            (TypeKind::Free(_), _) if !matches!(self.arena.get(expected), TypeKind::Free(_)) => {
                drop(self.unifier().unify(actual, expected));
            }
            (TypeKind::Union(actual_options), TypeKind::Union(expected_options)) => {
                self.infer_call_return_union_guidance(
                    &actual_options,
                    &expected_options,
                    seen_types,
                );
            }
            (TypeKind::Table(actual_table), TypeKind::Table(expected_table)) => {
                for (name, actual_property) in actual_table.properties {
                    if let Some(expected_property) = expected_table.properties.get(&name) {
                        self.infer_call_return_guidance_with(
                            actual_property.ty,
                            expected_property.ty,
                            seen_types,
                        );
                    }
                }
                if let (Some(actual_indexer), Some(expected_indexer)) =
                    (actual_table.indexer, expected_table.indexer)
                {
                    self.infer_call_return_guidance_with(
                        actual_indexer.key,
                        expected_indexer.key,
                        seen_types,
                    );
                    self.infer_call_return_guidance_with(
                        actual_indexer.value,
                        expected_indexer.value,
                        seen_types,
                    );
                }
            }
            _ => {}
        }
    }
    fn infer_call_return_union_guidance(
        &mut self,
        actual_options: &[TypeId],
        expected_options: &[TypeId],
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
    ) {
        if actual_options.len() != expected_options.len() {
            return;
        }
        let mut used_actual = vec![false; actual_options.len()];
        let mut used_expected = vec![false; expected_options.len()];
        let mut pairs = Vec::with_capacity(actual_options.len());

        for (actual_index, actual) in actual_options.iter().copied().enumerate() {
            if let Some(expected_index) = expected_options.iter().copied().enumerate().find_map(
                |(expected_index, expected)| {
                    (!used_expected[expected_index]
                        && self.arena.follow(actual) == self.arena.follow(expected))
                    .then_some(expected_index)
                },
            ) {
                used_actual[actual_index] = true;
                used_expected[expected_index] = true;
                pairs.push((actual, expected_options[expected_index]));
            }
        }

        for (actual_index, actual) in actual_options.iter().copied().enumerate() {
            if used_actual[actual_index] {
                continue;
            }
            let Some(expected_index) = expected_options.iter().copied().enumerate().find_map(
                |(expected_index, expected)| {
                    (!used_expected[expected_index]
                        && self.call_return_guidance_shapes_match(actual, expected))
                    .then_some(expected_index)
                },
            ) else {
                return;
            };
            used_actual[actual_index] = true;
            used_expected[expected_index] = true;
            pairs.push((actual, expected_options[expected_index]));
        }

        for (actual, expected) in pairs {
            self.infer_call_return_guidance_with(actual, expected, seen_types);
        }
    }
    fn call_return_guidance_shapes_match(&self, actual: TypeId, expected: TypeId) -> bool {
        let actual = self.arena.follow(actual);
        let expected = self.arena.follow(expected);
        if actual == expected {
            return true;
        }
        match (self.arena.get(actual), self.arena.get(expected)) {
            (TypeKind::Free(_), _) => !matches!(self.arena.get(expected), TypeKind::Free(_)),
            (TypeKind::Union(actual_options), TypeKind::Union(expected_options)) => {
                actual_options.len() == expected_options.len()
            }
            (TypeKind::Table(_), TypeKind::Table(_)) => true,
            _ => false,
        }
    }
    fn union_without_recursive_seed(&mut self, actual: TypeId, expected: TypeId) -> Option<TypeId> {
        let TypeKind::Union(options) = self.arena.get(actual).clone() else {
            return None;
        };
        let mut saw_expected = false;
        let mut seed = Vec::new();
        for option in options {
            if self.arena.follow(option) == expected {
                saw_expected = true;
            } else {
                seed.push(option);
            }
        }
        saw_expected.then(|| self.union_type(seed))
    }
    fn infer_call_argument_type(
        &mut self,
        argument: TypeId,
        parameter: TypeId,
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
        seen_packs: &mut BTreeSet<(TypePackId, TypePackId)>,
    ) {
        let argument = self.arena.follow(argument);
        let parameter = self.arena.follow(parameter);
        if argument == parameter {
            return;
        }
        if !seen_types.insert((argument, parameter)) {
            return;
        }
        match (
            self.arena.get(argument).clone(),
            self.arena.get(parameter).clone(),
        ) {
            (_, TypeKind::Free(_)) => {
                drop(self.unifier().unify(parameter, argument));
            }
            (_, TypeKind::Union(options)) => {
                for (argument, option) in self.inferable_union_parameter_options(argument, &options)
                {
                    self.infer_call_argument_type(argument, option, seen_types, seen_packs);
                }
            }
            (_, TypeKind::Intersection(options)) => {
                for option in options {
                    self.infer_call_argument_type(argument, option, seen_types, seen_packs);
                }
            }
            (TypeKind::Table(argument_table), TypeKind::Table(parameter_table)) => {
                self.infer_call_argument_table_instantiated_params(
                    argument_table.instantiated_type_params,
                    parameter_table.instantiated_type_params,
                    seen_types,
                    seen_packs,
                );
                self.infer_call_argument_table_instantiated_pack_params(
                    argument_table.instantiated_type_pack_params,
                    parameter_table.instantiated_type_pack_params,
                    seen_types,
                    seen_packs,
                );
                for (name, parameter_property) in parameter_table.properties {
                    if let Some(argument_property) = argument_table.properties.get(&name) {
                        self.infer_call_argument_type(
                            argument_property.ty,
                            parameter_property.ty,
                            seen_types,
                            seen_packs,
                        );
                    }
                }
                if let (Some(argument_indexer), Some(parameter_indexer)) =
                    (argument_table.indexer, parameter_table.indexer)
                {
                    self.infer_call_argument_type(
                        argument_indexer.key,
                        parameter_indexer.key,
                        seen_types,
                        seen_packs,
                    );
                    self.infer_call_argument_type(
                        argument_indexer.value,
                        parameter_indexer.value,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            (TypeKind::Function(argument_function), TypeKind::Function(parameter_function)) => {
                self.infer_call_argument_parameter_pack(
                    argument_function.arguments,
                    parameter_function.arguments,
                    seen_types,
                    seen_packs,
                );
                self.infer_call_argument_return_pack(
                    argument_function.returns,
                    parameter_function.returns,
                    seen_types,
                    seen_packs,
                );
            }
            _ => {}
        }
    }
    fn infer_call_argument_parameter_pack(
        &mut self,
        argument_parameters: TypePackId,
        expected_parameters: TypePackId,
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
        seen_packs: &mut BTreeSet<(TypePackId, TypePackId)>,
    ) {
        let argument_parameters = self.arena.follow_pack(argument_parameters);
        let expected_parameters = self.arena.follow_pack(expected_parameters);
        if !seen_packs.insert((argument_parameters, expected_parameters)) {
            return;
        }
        if let Some(parameters) = CallParameterPack::from_list(self.arena, expected_parameters) {
            self.infer_call_argument_bindings_with(
                argument_parameters,
                parameters,
                false,
                seen_types,
                seen_packs,
            );
        } else {
            drop(
                self.unifier()
                    .constrain_pack_subtype(argument_parameters, expected_parameters),
            );
        }
    }
    fn infer_call_argument_return_pack(
        &mut self,
        argument_returns: TypePackId,
        parameter_returns: TypePackId,
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
        seen_packs: &mut BTreeSet<(TypePackId, TypePackId)>,
    ) {
        let argument_returns = self.arena.follow_pack(argument_returns);
        let parameter_returns = self.arena.follow_pack(parameter_returns);
        if !seen_packs.insert((argument_returns, parameter_returns)) {
            return;
        }
        if let Some(parameters) = CallParameterPack::from_list(self.arena, parameter_returns) {
            self.infer_call_argument_bindings_with(
                argument_returns,
                parameters,
                false,
                seen_types,
                seen_packs,
            );
        } else {
            drop(
                self.unifier()
                    .constrain_pack_subtype(argument_returns, parameter_returns),
            );
        }
    }
    fn infer_call_argument_table_instantiated_params(
        &mut self,
        arguments: Vec<TypeId>,
        parameters: Vec<TypeId>,
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
        seen_packs: &mut BTreeSet<(TypePackId, TypePackId)>,
    ) {
        if arguments.len() != parameters.len() {
            return;
        }
        for (argument, parameter) in arguments.into_iter().zip(parameters) {
            self.infer_call_argument_type(argument, parameter, seen_types, seen_packs);
        }
    }
    /// Pack analog of `infer_call_argument_table_instantiated_params`: an
    /// instantiated generic-alias pack argument (`Phantom<A...>` applied to
    /// `Phantom<number>`) binds the signature's fresh free pack even when the
    /// alias body never mentions the pack, so the call's other uses of `A...`
    /// see the argument's instantiation instead of staying unconstrained.
    fn infer_call_argument_table_instantiated_pack_params(
        &mut self,
        arguments: Vec<TypePackId>,
        parameters: Vec<TypePackId>,
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
        seen_packs: &mut BTreeSet<(TypePackId, TypePackId)>,
    ) {
        if arguments.len() != parameters.len() {
            return;
        }
        for (argument, parameter) in arguments.into_iter().zip(parameters) {
            self.infer_call_argument_instantiated_pack(argument, parameter, seen_types, seen_packs);
        }
    }
    fn infer_call_argument_instantiated_pack(
        &mut self,
        argument: TypePackId,
        parameter: TypePackId,
        seen_types: &mut BTreeSet<(TypeId, TypeId)>,
        seen_packs: &mut BTreeSet<(TypePackId, TypePackId)>,
    ) {
        let argument = self.arena.follow_pack(argument);
        let parameter = self.arena.follow_pack(parameter);
        if argument == parameter {
            return;
        }
        if !seen_packs.insert((argument, parameter)) {
            return;
        }
        match (
            self.arena.get_pack(argument).clone(),
            self.arena.get_pack(parameter).clone(),
        ) {
            (_, TypePackKind::Free { .. }) => {
                drop(self.unifier().unify_pack(parameter, argument));
            }
            (
                TypePackKind::List {
                    types: argument_types,
                    tail: argument_tail,
                },
                TypePackKind::List {
                    types: parameter_types,
                    tail: parameter_tail,
                },
            ) => {
                let argument_pack = self.arena.flatten_list_pack_from_parts(
                    argument,
                    argument_types,
                    argument_tail,
                );
                let parameter_pack = self.arena.flatten_list_pack_from_parts(
                    parameter,
                    parameter_types,
                    parameter_tail,
                );
                for (argument, parameter) in
                    argument_pack.types.into_iter().zip(parameter_pack.types)
                {
                    self.infer_call_argument_type(argument, parameter, seen_types, seen_packs);
                }
                if let (Some(argument_tail), Some(parameter_tail)) =
                    (argument_pack.tail, parameter_pack.tail)
                {
                    self.infer_call_argument_instantiated_pack(
                        argument_tail,
                        parameter_tail,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            (TypePackKind::Variadic { ty: argument }, TypePackKind::Variadic { ty: parameter }) => {
                self.infer_call_argument_type(argument, parameter, seen_types, seen_packs);
            }
            _ => {}
        }
    }
    fn type_is_unconstrained_free(&self, ty: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(ty)),
            TypeKind::Free(crate::types::TypeVariable {
                lower_bound: None,
                upper_bound: None,
                ..
            })
        )
    }
    fn type_is_or_contains_free(&self, ty: TypeId) -> bool {
        self.type_is_or_contains_free_with(ty, &mut Vec::new(), &mut Vec::new())
    }
    fn type_is_or_contains_free_with(
        &self,
        ty: TypeId,
        seen_types: &mut Vec<TypeId>,
        seen_packs: &mut Vec<TypePackId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if seen_types.contains(&ty) {
            return false;
        }
        seen_types.push(ty);
        match self.arena.get(ty) {
            TypeKind::Free(_) => true,
            TypeKind::Union(options) | TypeKind::Intersection(options) => options
                .iter()
                .any(|ty| self.type_is_or_contains_free_with(*ty, seen_types, seen_packs)),
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.type_is_or_contains_free_with(*inner, seen_types, seen_packs)
            }
            TypeKind::Function(function) => {
                self.pack_is_or_contains_free_with(function.arguments, seen_types, seen_packs)
                    || self.pack_is_or_contains_free_with(function.returns, seen_types, seen_packs)
            }
            TypeKind::Table(table) => {
                table
                    .instantiated_type_params
                    .iter()
                    .chain(table.properties.values().map(|property| &property.ty))
                    .any(|ty| self.type_is_or_contains_free_with(*ty, seen_types, seen_packs))
                    || table.indexer.iter().any(|indexer| {
                        self.type_is_or_contains_free_with(indexer.key, seen_types, seen_packs)
                            || self.type_is_or_contains_free_with(
                                indexer.value,
                                seen_types,
                                seen_packs,
                            )
                    })
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_is_or_contains_free_with(*table, seen_types, seen_packs)
                    || self.type_is_or_contains_free_with(*metatable, seen_types, seen_packs)
            }
            TypeKind::TypeFunctionInstance { arguments, .. } => arguments
                .iter()
                .any(|ty| self.type_is_or_contains_free_with(*ty, seen_types, seen_packs)),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }
    fn inferable_union_parameter_options(
        &mut self,
        argument: TypeId,
        options: &[TypeId],
    ) -> Vec<(TypeId, TypeId)> {
        let Some(argument) = self.argument_remainder_after_concrete_options(argument, options)
        else {
            return Vec::new();
        };
        let free_options = options
            .iter()
            .copied()
            .filter(|option| {
                self.type_is_or_contains_free(*option)
                    && self.option_shape_can_accept_argument(argument, *option)
            })
            .collect::<Vec<_>>();
        if let [option] = free_options.as_slice() {
            return vec![(argument, *option)];
        }

        let mut pairs = Vec::new();
        for argument_option in self.arena.union_options(argument) {
            let candidates = free_options
                .iter()
                .copied()
                .filter(|option| self.option_shape_can_accept_argument(argument_option, *option))
                .collect::<Vec<_>>();
            let specific = candidates
                .iter()
                .copied()
                .filter(|option| !self.option_is_bare_free(*option))
                .collect::<Vec<_>>();
            let candidates = if specific.is_empty() {
                candidates
            } else {
                specific
            };
            let [option] = candidates.as_slice() else {
                return Vec::new();
            };
            pairs.push((argument_option, *option));
        }
        pairs
    }
    fn option_is_bare_free(&self, option: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(option)),
            TypeKind::Free(_)
                | TypeKind::Any
                | TypeKind::Unknown
                | TypeKind::Error
                | TypeKind::Blocked(_)
        )
    }
    fn argument_remainder_after_concrete_options(
        &mut self,
        argument: TypeId,
        options: &[TypeId],
    ) -> Option<TypeId> {
        let concrete_options = options
            .iter()
            .copied()
            .filter(|option| !self.type_is_or_contains_free(*option))
            .collect::<Vec<_>>();
        if concrete_options.is_empty() {
            return Some(argument);
        }

        let unmatched = self
            .arena
            .union_options(argument)
            .into_iter()
            .filter(|argument_option| {
                let argument_option = self.arena.follow(*argument_option);
                !concrete_options.iter().copied().any(|option| {
                    Subtyper::new(self.arena)
                        .is_subtype(argument_option, option)
                        .is_ok()
                })
            })
            .collect::<Vec<_>>();

        match unmatched.as_slice() {
            [] => None,
            [only] => Some(*only),
            _ => Some(self.union_type(unmatched)),
        }
    }
    fn option_shape_can_accept_argument(&self, argument: TypeId, option: TypeId) -> bool {
        self.option_shape_can_accept_argument_with_seen(argument, option, &mut BTreeSet::new())
    }
    fn option_shape_can_accept_argument_with_seen(
        &self,
        argument: TypeId,
        option: TypeId,
        seen: &mut BTreeSet<(TypeId, TypeId)>,
    ) -> bool {
        if !seen.insert((self.arena.follow(argument), self.arena.follow(option))) {
            // A cyclic union/bound chain revisited this pair (fuzz-found
            // stack overflow). Answer optimistically, matching the `Free`
            // and `Any` arms of this heuristic shape probe.
            return true;
        }
        match (
            self.arena.get(self.arena.follow(argument)),
            self.arena.get(self.arena.follow(option)),
        ) {
            (_, TypeKind::Free(_)) => true,
            (TypeKind::Function(_), TypeKind::Function(_)) => true,
            (TypeKind::Table(_), TypeKind::Table(_)) => true,
            (TypeKind::Primitive(argument), TypeKind::Primitive(option)) if argument == option => {
                true
            }
            (TypeKind::Singleton(singleton), TypeKind::Singleton(option))
                if singleton == option =>
            {
                true
            }
            (TypeKind::Singleton(singleton), TypeKind::Primitive(primitive)) => {
                matches!(
                    (singleton, primitive),
                    (SingletonType::Boolean(_), PrimitiveType::Boolean)
                        | (SingletonType::String(_), PrimitiveType::String)
                )
            }
            (TypeKind::Union(arguments), _) => arguments.iter().any(|argument| {
                self.option_shape_can_accept_argument_with_seen(*argument, option, seen)
            }),
            (_, TypeKind::Union(options) | TypeKind::Intersection(options)) => {
                options.iter().any(|option| {
                    self.option_shape_can_accept_argument_with_seen(argument, *option, seen)
                })
            }
            (_, TypeKind::Bound(bound)) => {
                self.option_shape_can_accept_argument_with_seen(argument, *bound, seen)
            }
            (TypeKind::Bound(bound), _) => {
                self.option_shape_can_accept_argument_with_seen(*bound, option, seen)
            }
            (
                TypeKind::Any
                | TypeKind::Unknown
                | TypeKind::Error
                | TypeKind::Blocked(_)
                | TypeKind::Free(_),
                _,
            )
            | (_, TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_)) => {
                true
            }
            _ => false,
        }
    }
    fn pack_is_or_contains_free_with(
        &self,
        pack: TypePackId,
        seen_types: &mut Vec<TypeId>,
        seen_packs: &mut Vec<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if seen_packs.contains(&pack) {
            return false;
        }
        seen_packs.push(pack);
        match self.arena.get_pack(pack) {
            TypePackKind::Free { .. } => true,
            TypePackKind::List { types, tail } => {
                types
                    .iter()
                    .any(|ty| self.type_is_or_contains_free_with(*ty, seen_types, seen_packs))
                    || tail.is_some_and(|tail| {
                        self.pack_is_or_contains_free_with(tail, seen_types, seen_packs)
                    })
            }
            TypePackKind::Variadic { ty } => {
                self.type_is_or_contains_free_with(*ty, seen_types, seen_packs)
            }
            TypePackKind::Bound(bound) => {
                self.pack_is_or_contains_free_with(*bound, seen_types, seen_packs)
            }
            TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }
    fn ambiguous_union_call_returns(
        &mut self,
        callee: TypeId,
        error: &OverloadError,
    ) -> Option<TypePackId> {
        let OverloadError::Ambiguous { candidates } = error else {
            return None;
        };
        if !matches!(
            self.arena.get(self.arena.follow(callee)),
            TypeKind::Union(_)
        ) {
            return None;
        }

        let mut candidate_returns = Vec::new();
        let mut return_count = None;
        for candidate in candidates {
            let TypeKind::Function(function) =
                self.arena.get(self.arena.follow(*candidate)).clone()
            else {
                return None;
            };
            let returns = self.arena.normalize_pack(function.returns);
            if returns.tail.is_some() {
                return None;
            }
            match return_count {
                Some(count) if count != returns.types.len() => return None,
                None => return_count = Some(returns.types.len()),
                _ => {}
            }
            candidate_returns.push(returns.types);
        }
        let return_count = return_count?;
        let mut returns = Vec::with_capacity(return_count);
        for index in 0..return_count {
            let options = candidate_returns
                .iter()
                .map(|candidate| candidate[index])
                .collect::<Vec<_>>();
            returns.push(self.union_type(options));
        }
        Some(self.arena.alloc_pack(TypePackKind::List {
            types: returns,
            tail: None,
        }))
    }
    fn uninhabited_type_function_in_pack(&self, pack: TypePackId) -> Option<String> {
        // The reduction probe needs a mutable arena; one lazily-built scratch
        // clone serves the whole walk instead of one clone per visited
        // type-function node.
        self.uninhabited_type_function_in_pack_with(
            pack,
            &mut None,
            &mut Vec::new(),
            &mut Vec::new(),
        )
    }
    fn uninhabited_type_function_in_pack_with(
        &self,
        pack: TypePackId,
        scratch: &mut Option<Arena>,
        seen_types: &mut Vec<TypeId>,
        seen_packs: &mut Vec<TypePackId>,
    ) -> Option<String> {
        let pack = self.arena.follow_pack(pack);
        if seen_packs.contains(&pack) {
            return None;
        }
        seen_packs.push(pack);
        match self.arena.get_pack(pack).clone() {
            TypePackKind::List { types, tail } => {
                for ty in types {
                    if let Some(instance) = self
                        .uninhabited_type_function_in_type_with(ty, scratch, seen_types, seen_packs)
                    {
                        return Some(instance);
                    }
                }
                tail.and_then(|tail| {
                    self.uninhabited_type_function_in_pack_with(
                        tail, scratch, seen_types, seen_packs,
                    )
                })
            }
            TypePackKind::Variadic { ty } => {
                self.uninhabited_type_function_in_type_with(ty, scratch, seen_types, seen_packs)
            }
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => None,
        }
    }
    fn uninhabited_type_function_in_type_with(
        &self,
        ty: TypeId,
        scratch: &mut Option<Arena>,
        seen_types: &mut Vec<TypeId>,
        seen_packs: &mut Vec<TypePackId>,
    ) -> Option<String> {
        let ty = self.arena.follow(ty);
        if seen_types.contains(&ty) {
            return None;
        }
        seen_types.push(ty);
        match self.arena.get(ty).clone() {
            TypeKind::TypeFunctionInstance { name, arguments } => {
                let probe = scratch.get_or_insert_with(|| self.arena.clone());
                if matches!(
                    TypeFunctionRuntime::new().reduce_allocating(probe, &name, &arguments),
                    Reduction::Reduced(reduced)
                        if matches!(probe.get(probe.follow(reduced)), TypeKind::Never)
                ) {
                    return Some(self.arena.summary(ty));
                }
                for argument in arguments {
                    if let Some(instance) = self.uninhabited_type_function_in_type_with(
                        argument, scratch, seen_types, seen_packs,
                    ) {
                        return Some(instance);
                    }
                }
                None
            }
            TypeKind::Function(function) => self
                .uninhabited_type_function_in_pack_with(
                    function.arguments,
                    scratch,
                    seen_types,
                    seen_packs,
                )
                .or_else(|| {
                    self.uninhabited_type_function_in_pack_with(
                        function.returns,
                        scratch,
                        seen_types,
                        seen_packs,
                    )
                }),
            TypeKind::Table(table) => {
                for ty in table
                    .instantiated_type_params
                    .into_iter()
                    .chain(table.properties.values().map(|property| property.ty))
                {
                    if let Some(instance) = self
                        .uninhabited_type_function_in_type_with(ty, scratch, seen_types, seen_packs)
                    {
                        return Some(instance);
                    }
                }
                table.indexer.and_then(|indexer| {
                    self.uninhabited_type_function_in_type_with(
                        indexer.key,
                        scratch,
                        seen_types,
                        seen_packs,
                    )
                    .or_else(|| {
                        self.uninhabited_type_function_in_type_with(
                            indexer.value,
                            scratch,
                            seen_types,
                            seen_packs,
                        )
                    })
                })
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => self
                .uninhabited_type_function_in_type_with(table, scratch, seen_types, seen_packs)
                .or_else(|| {
                    self.uninhabited_type_function_in_type_with(
                        metatable, scratch, seen_types, seen_packs,
                    )
                }),
            TypeKind::Union(types) | TypeKind::Intersection(types) => {
                for ty in types {
                    if let Some(instance) = self
                        .uninhabited_type_function_in_type_with(ty, scratch, seen_types, seen_packs)
                    {
                        return Some(instance);
                    }
                }
                None
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.uninhabited_type_function_in_type_with(inner, scratch, seen_types, seen_packs)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Free(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any
            | TypeKind::Blocked(_) => None,
        }
    }
}
