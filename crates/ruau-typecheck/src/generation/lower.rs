//! Expression constraint generation for single-module checking.

use std::collections::{BTreeMap, BTreeSet};

use ruau_ast::{
    Location,
    syntax::{
        DeclaredClassProp, Expr, TableIndexer as AstTableIndexer, Type, TypeList, TypePack,
        TypeParameter,
    },
};

use crate::{
    diagnostics::{Diagnostic, DiagnosticCategory, DiagnosticLocation},
    generation::{
        state::ExpressionConstraintGenerator,
        type_function_eval::{TypeFunctionEvaluation, TypeFunctionEvaluator, TypeFunctionValue},
    },
    generic_alias,
    magic_types::LUAU_FORCE_CONSTRAINT_SOLVING_INCOMPLETE,
    scopes::{ScopeId, TypeBindingKind},
    type_function::{
        Reduction, SETMETATABLE_TYPE_FUNCTION, TypeFunctionRuntime, is_builtin_type_function,
        setmetatable_type_function_arguments,
    },
    types::{
        BlockedType, FunctionType, GenericType, GenericTypePack, SingletonType, TableAliasIdentity,
        TableIndexer, TableProperty, TableState, TableType, TypeId, TypeKind, TypeLevel,
        TypePackId, TypePackKind,
    },
};

const HIDDEN_ERROR_TYPE_ALIAS_TARGET: &str = "__ruau_error";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TypePackLoweringContext {
    TypeAnnotation,
    FunctionVarargAnnotation,
}

struct GenericAliasSubstitutions {
    types: BTreeMap<String, TypeId>,
    packs: BTreeMap<String, TypePackId>,
    instantiated_type_params: Vec<TypeId>,
    instantiated_pack_params: Vec<TypePackId>,
}

/// The generic-parameter surface of one source type alias: parallel
/// name/default lists for its type parameters and its type-pack parameters.
struct AliasGenerics<'a> {
    names: &'a [String],
    defaults: &'a [Option<Type>],
    pack_names: &'a [String],
    pack_defaults: &'a [Option<TypePack>],
}

fn type_location(ty: &Type) -> DiagnosticLocation {
    DiagnosticLocation::from_opt(ty.location())
}

#[allow(clippy::multiple_inherent_impl)]
impl<'a> ExpressionConstraintGenerator<'a> {
    pub(crate) fn with_type_alias_frame<T>(
        &mut self,
        alias_name: String,
        lower: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.alias_lowering.type_alias_stack.push(alias_name);
        let lowered = lower(self);
        self.alias_lowering.type_alias_stack.pop();
        lowered
    }

    pub(crate) fn with_type_alias_definition_frame<T>(
        &mut self,
        alias_name: String,
        alias_identity: TableAliasIdentity,
        lower: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.alias_lowering.type_alias_stack.push(alias_name);
        self.alias_lowering
            .type_alias_definition_stack
            .push(alias_identity);
        let lowered = lower(self);
        self.alias_lowering.type_alias_definition_stack.pop();
        self.alias_lowering.type_alias_stack.pop();
        lowered
    }

    pub(crate) fn with_generic_type_substitution_frame<T>(
        &mut self,
        types: BTreeMap<String, TypeId>,
        packs: BTreeMap<String, TypePackId>,
        lower: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.alias_lowering.generic_type_substitutions.push(types);
        self.alias_lowering
            .generic_type_pack_substitutions
            .push(packs);
        let lowered = lower(self);
        self.alias_lowering.generic_type_pack_substitutions.pop();
        self.alias_lowering.generic_type_substitutions.pop();
        lowered
    }

    pub(crate) fn with_function_signature_lowering<T>(
        &mut self,
        lower: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.alias_lowering.type_alias_function_depth += 1;
        let lowered = lower(self);
        self.alias_lowering.type_alias_function_depth -= 1;
        lowered
    }

    pub(crate) fn with_generic_alias_type_arguments<T>(
        &mut self,
        lower: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.alias_lowering.generic_alias_type_argument_depth += 1;
        let lowered = lower(self);
        self.alias_lowering.generic_alias_type_argument_depth -= 1;
        lowered
    }

    pub(crate) fn lower_type(&mut self, scope: ScopeId, ty: &Type) -> TypeId {
        let primitives = self.primitives();
        match ty {
            Type::Reference {
                location: reference_location,
                prefix,
                name,
                name_location,
                parameters,
                ..
            } => {
                let name = name.as_str();
                let qualified_name = prefix
                    .as_ref()
                    .map(|prefix| format!("{}.{}", prefix.as_str(), name));
                let lookup_name = qualified_name.as_deref().unwrap_or(name);
                let has_parameter_list = generic_alias::type_reference_has_parameter_list(
                    *reference_location,
                    *name_location,
                );
                if prefix.is_none()
                    && parameters.is_empty()
                    && let Some(ty) = self.generic_type_substitution(name)
                {
                    return ty;
                }
                if prefix.is_none()
                    && parameters.is_empty()
                    && name == LUAU_FORCE_CONSTRAINT_SOLVING_INCOMPLETE
                {
                    self.generated.diagnostics.push(
                        Diagnostic::error(
                            DiagnosticCategory::Constraint,
                            name_location
                                .as_ref()
                                .copied()
                                .map(DiagnosticLocation::from)
                                .unwrap_or_else(DiagnosticLocation::missing),
                        )
                        .with_context(format!(
                            "{LUAU_FORCE_CONSTRAINT_SOLVING_INCOMPLETE} forced an incomplete \
                             constraint-solving diagnostic"
                        ))
                        .with_typed(crate::diagnostics::Payload::ConstraintSolvingIncompleteForced),
                    );
                    return primitives.any;
                }
                if prefix.is_none()
                    && parameters.is_empty()
                    && name == HIDDEN_ERROR_TYPE_ALIAS_TARGET
                {
                    return primitives.error;
                }
                if prefix.is_none()
                    && name == "Not"
                    && parameters.len() == 1
                    && let TypeParameter::Type(target) = &parameters[0]
                {
                    let target = self.lower_type(scope, target);
                    return self.arena.alloc(TypeKind::Negation(target));
                }
                if let Some((binding_scope, binding)) =
                    self.input.scopes.lookup_type_with_scope(scope, lookup_name)
                {
                    if let Some(ty) = binding.ty {
                        return ty;
                    }
                    if binding.kind == TypeBindingKind::GenericParameter {
                        let key = (binding_scope, lookup_name.to_owned());
                        if let Some(existing) = self.alias_lowering.generic_type_cache.get(&key) {
                            return *existing;
                        }
                        let ty = self.arena.alloc(TypeKind::Generic(GenericType {
                            name: lookup_name.to_owned(),
                            level: TypeLevel(0),
                        }));
                        self.alias_lowering.generic_type_cache.insert(key, ty);
                        return ty;
                    }
                    if binding.kind == TypeBindingKind::GenericPackParameter {
                        self.report_generic_pack_used_as_type(
                            lookup_name,
                            name_location
                                .as_ref()
                                .copied()
                                .map(DiagnosticLocation::from),
                        );
                        return primitives.error;
                    }
                    if matches!(
                        binding.kind,
                        TypeBindingKind::Class | TypeBindingKind::DeclaredClass
                    ) {
                        let super_name = binding.class_super_name.clone();
                        let props = binding.class_props.clone();
                        if self
                            .alias_lowering
                            .type_alias_stack
                            .iter()
                            .any(|alias| alias == lookup_name)
                        {
                            let key = (binding_scope, lookup_name.to_owned());
                            if let Some(placeholder) = self
                                .alias_lowering
                                .class_lowering_placeholders
                                .get(&key)
                                .copied()
                            {
                                return placeholder;
                            }
                            return self.empty_extern_type(lookup_name, &[]);
                        }
                        let indexer = binding.class_indexer.clone();
                        let ty = self.with_type_alias_frame(lookup_name.to_owned(), |this| {
                            this.lower_class_binding(
                                binding_scope,
                                lookup_name,
                                &super_name,
                                props,
                                indexer,
                            )
                        });
                        return ty;
                    }
                    if binding.kind == TypeBindingKind::TypeFunction {
                        let type_function = binding.type_function.clone();
                        let location = name_location.map(DiagnosticLocation::from);
                        if has_parameter_list || !parameters.is_empty() {
                            if let Some(func) = type_function.as_ref() {
                                match self.reduce_user_type_function(
                                    scope,
                                    lookup_name,
                                    func,
                                    parameters,
                                    location,
                                ) {
                                    TypeFunctionEvaluation::Reduced(reduced) => return reduced,
                                    TypeFunctionEvaluation::Uninhabited => {
                                        let instance = self.lower_type_function_instance(
                                            scope,
                                            lookup_name,
                                            parameters,
                                            location,
                                        );
                                        self.report_uninhabited_type_function_diagnostic(
                                            instance, location,
                                        );
                                        return instance;
                                    }
                                    TypeFunctionEvaluation::RuntimeError => {
                                        return self.lower_type_function_instance(
                                            scope,
                                            lookup_name,
                                            parameters,
                                            location,
                                        );
                                    }
                                    TypeFunctionEvaluation::Deferred => {}
                                }
                            }
                            return self.lower_type_function_instance(
                                scope,
                                lookup_name,
                                parameters,
                                location,
                            );
                        }
                        // A bare `foo` reference (no `<...>`) leaves the type
                        // function unapplied, an error upstream. Still reduce a
                        // zero-argument function so the surrounding annotation
                        // sees its result type and any further mismatch surfaces.
                        self.report_unapplied_type_function(lookup_name, location);
                        if let Some(func) = type_function.as_ref() {
                            match self.reduce_user_type_function(
                                scope,
                                lookup_name,
                                func,
                                parameters,
                                location,
                            ) {
                                TypeFunctionEvaluation::Reduced(reduced) => return reduced,
                                TypeFunctionEvaluation::Uninhabited
                                | TypeFunctionEvaluation::RuntimeError
                                | TypeFunctionEvaluation::Deferred => {}
                            }
                        }
                        return primitives.error;
                    }
                    let recursive_generic_alias = binding.alias.as_ref().is_some_and(|alias| {
                        Self::generic_alias_allows_recursive_generic_type_arguments(
                            lookup_name,
                            &binding.generic_pack_names,
                            alias,
                        )
                    });
                    let alias_identity = binding.alias_identity.clone().unwrap_or_else(|| {
                        self.input.scopes.alias_identity(binding_scope, lookup_name)
                    });
                    if !(binding.alias_has_generics && recursive_generic_alias)
                        && self
                            .alias_lowering
                            .type_alias_definition_stack
                            .iter()
                            .any(|alias| alias == &alias_identity)
                    {
                        return self
                            .alias_lowering
                            .type_alias_cache
                            .get(&alias_identity)
                            .copied()
                            .unwrap_or(primitives.any);
                    }
                    if !binding.alias_has_generics
                        && let Some(alias) = binding.alias.clone()
                    {
                        return self.lower_non_generic_alias(
                            scope,
                            lookup_name,
                            alias_identity,
                            &alias,
                        );
                    }
                    if binding.alias_has_generics
                        && let Some(alias) = binding.alias.clone()
                    {
                        let Some(substitutions) = self.generic_alias_substitutions(
                            scope,
                            lookup_name,
                            &AliasGenerics {
                                names: &binding.generic_names,
                                defaults: &binding.generic_defaults,
                                pack_names: &binding.generic_pack_names,
                                pack_defaults: &binding.generic_pack_defaults,
                            },
                            parameters,
                            generic_alias::type_reference_has_parameter_list(
                                *reference_location,
                                *name_location,
                            ),
                            name_location.map(DiagnosticLocation::from),
                            recursive_generic_alias,
                        ) else {
                            return primitives.any;
                        };
                        let instantiated_type_params =
                            substitutions.instantiated_type_params.clone();
                        let instantiated_pack_params =
                            substitutions.instantiated_pack_params.clone();
                        let placeholder = if recursive_generic_alias {
                            let cache_key = (
                                alias_identity.clone(),
                                instantiated_type_params.clone(),
                                instantiated_pack_params.clone(),
                            );
                            if let Some(cached) = self
                                .alias_lowering
                                .generic_type_alias_cache
                                .get(&cache_key)
                                .copied()
                            {
                                return cached;
                            }
                            let placeholder = self.arena.alloc(TypeKind::Blocked(BlockedType {
                                reason: Some(format!("generic type alias {lookup_name}")),
                            }));
                            self.alias_lowering
                                .generic_type_alias_cache
                                .insert(cache_key, placeholder);
                            Some(placeholder)
                        } else {
                            None
                        };
                        let ty = self.with_type_alias_definition_frame(
                            lookup_name.to_owned(),
                            alias_identity.clone(),
                            |this| {
                                this.with_generic_type_substitution_frame(
                                    substitutions.types,
                                    substitutions.packs,
                                    |this| this.lower_type(scope, &alias),
                                )
                            },
                        );
                        let display_name = binding.display_name.as_deref().unwrap_or(lookup_name);
                        let (ty, _) = self.reduce_alias_type_function(ty);
                        if let Some(placeholder) = placeholder
                            && self.type_has_transparent_alias_occurrence(placeholder, ty)
                        {
                            self.report_recursive_type_alias(display_name, &alias);
                            self.arena.replace(placeholder, TypeKind::Error);
                            return placeholder;
                        }
                        let ty = self.name_type_alias_result(
                            ty,
                            display_name,
                            Some(alias_identity),
                            instantiated_type_params,
                            instantiated_pack_params,
                        );
                        if let Some(placeholder) = placeholder {
                            let replacement = self.arena.get(self.arena.follow(ty)).clone();
                            self.arena.replace(placeholder, replacement);
                            return placeholder;
                        }
                        return ty;
                    }
                }
                if prefix.is_none()
                    && (has_parameter_list || !parameters.is_empty())
                    && is_builtin_type_function(name)
                {
                    return self.lower_type_function_instance(
                        scope,
                        name,
                        parameters,
                        name_location.map(DiagnosticLocation::from),
                    );
                }
                self.generated.diagnostics.push(Diagnostic::unknown_type(
                    lookup_name,
                    name_location
                        .as_ref()
                        .copied()
                        .map(DiagnosticLocation::from)
                        .unwrap_or_else(DiagnosticLocation::missing),
                ));
                primitives.error
            }
            Type::SingletonString { value, .. } => self
                .arena
                .alloc(TypeKind::Singleton(SingletonType::String(value.clone()))),
            Type::SingletonBool { value, .. } => self
                .arena
                .alloc(TypeKind::Singleton(SingletonType::Boolean(*value))),
            Type::Group { inner, .. } => self.lower_type(scope, inner),
            Type::Union { types, .. } => {
                let lowered = types
                    .iter()
                    .map(|ty| self.lower_type(scope, ty))
                    .collect::<Vec<_>>();
                self.union_type(lowered)
            }
            Type::Intersection { types, .. } => {
                let lowered = types
                    .iter()
                    .map(|ty| self.lower_type(scope, ty))
                    .collect::<Vec<_>>();
                self.arena.alloc(TypeKind::Intersection(lowered))
            }
            Type::Function {
                attributes,
                generics,
                generic_packs,
                arg_types,
                arg_names,
                return_types,
                ..
            } => {
                let function_scope = self.enter_child(scope);
                let (function_generics, type_substitutions) =
                    self.function_type_generic_substitutions(generics);
                let (function_generic_packs, pack_substitutions) =
                    self.function_type_generic_pack_substitutions(generic_packs);
                let (args, returns) = self.with_generic_type_substitution_frame(
                    type_substitutions,
                    pack_substitutions,
                    |this| {
                        this.with_function_signature_lowering(|this| {
                            let args = this.lower_type_list(function_scope, arg_types);
                            let returns = this.lower_type_pack(function_scope, return_types);
                            (args, returns)
                        })
                    },
                );
                let mut function = FunctionType::new(args, returns);
                function.generics = function_generics;
                function.generic_packs = function_generic_packs;
                function.argument_names = arg_names
                    .iter()
                    .map(|name| name.as_ref().map(|name| name.name.as_str().to_owned()))
                    .collect();
                function.is_checked = attributes
                    .iter()
                    .any(|attribute| attribute.name.as_str() == "checked");
                self.arena.alloc(TypeKind::Function(function))
            }
            Type::Table { props, indexer, .. } => {
                let mut table = TableType::new(TableState::Sealed);
                for prop in props {
                    let prop_ty = self.lower_type(scope, &prop.prop_type);
                    let mut property = TableProperty::new(prop_ty)
                        .with_location(prop.location.map(DiagnosticLocation::from));
                    property.read_only = prop.read_only;
                    property.write_only = prop.write_only;
                    table
                        .properties
                        .insert(prop.name.as_str().to_owned(), property);
                }
                if let Some(indexer) = indexer {
                    table.indexer = Some(crate::types::TableIndexer {
                        key: self.lower_type(scope, &indexer.index_type),
                        value: self.lower_type(scope, &indexer.result_type),
                        read_only: indexer.read_only,
                    });
                }
                self.arena.alloc(TypeKind::Table(table))
            }
            Type::Typeof { expr, location, .. } => {
                let ty = self.expr_type(scope, expr);
                let ty = if self.record_typeof_nil_snapshot(expr, ty) {
                    primitives.nil
                } else {
                    ty
                };
                self.report_uninhabited_type_function_diagnostics_for_type(
                    ty,
                    location.map(DiagnosticLocation::from),
                );
                ty
            }
            Type::Optional { .. } => primitives.nil,
            Type::Error { .. } => primitives.error,
        }
    }

    fn lower_type_function_instance(
        &mut self,
        scope: ScopeId,
        name: &str,
        parameters: &[TypeParameter],
        location: Option<DiagnosticLocation>,
    ) -> TypeId {
        let Some(arguments) = self.lower_type_function_arguments(scope, name, parameters, location)
        else {
            return self.primitives().error;
        };
        self.arena.alloc(TypeKind::TypeFunctionInstance {
            name: name.to_owned(),
            arguments,
        })
    }

    fn reduce_user_type_function(
        &mut self,
        scope: ScopeId,
        name: &str,
        func: &Expr,
        parameters: &[TypeParameter],
        location: Option<DiagnosticLocation>,
    ) -> TypeFunctionEvaluation {
        let Some(arguments) = self.lower_type_function_arguments(scope, name, parameters, location)
        else {
            return TypeFunctionEvaluation::Deferred;
        };
        self.reduce_user_type_function_with_arguments(scope, name, func, arguments, location)
    }

    pub(crate) fn reduce_user_type_function_with_arguments(
        &mut self,
        scope: ScopeId,
        name: &str,
        func: &Expr,
        arguments: Vec<TypeId>,
        location: Option<DiagnosticLocation>,
    ) -> TypeFunctionEvaluation {
        let Expr::Function {
            args: function_args,
            body,
            ..
        } = func
        else {
            return TypeFunctionEvaluation::Deferred;
        };
        if function_args.len() != arguments.len() {
            return TypeFunctionEvaluation::Deferred;
        }

        let frame_arguments = arguments
            .iter()
            .map(|ty| self.arena.follow(*ty))
            .collect::<Vec<_>>();
        if let Err(limit) = self
            .type_function_evaluation
            .enter(scope, name, frame_arguments)
        {
            self.report_type_function_runtime_error_at(limit.reason(), location);
            return TypeFunctionEvaluation::RuntimeError;
        }
        let arguments_concrete = arguments
            .iter()
            .all(|ty| self.generic_alias_argument_is_concrete(*ty, false));
        let env = function_args
            .iter()
            .zip(arguments)
            .map(|(arg, ty)| (arg.name.as_str().to_owned(), TypeFunctionValue::Type(ty)))
            .collect::<BTreeMap<_, _>>();
        let evaluation =
            TypeFunctionEvaluator::new(self, scope, env, location).run(body, arguments_concrete);
        self.type_function_evaluation.leave();
        evaluation
    }

    pub(crate) fn name_type_alias_result(
        &mut self,
        ty: TypeId,
        alias_name: &str,
        alias_identity: Option<TableAliasIdentity>,
        instantiated_type_params: Vec<TypeId>,
        instantiated_type_pack_params: Vec<TypePackId>,
    ) -> TypeId {
        let (ty, _) = self.reduce_alias_type_function(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Table(mut table) => {
                if !self.alias_result_is_instantiated_self(&table, ty, &instantiated_type_params) {
                    table.name = Some(alias_name.to_owned());
                    table.alias_identity = alias_identity;
                }
                table.instantiated_type_params = instantiated_type_params;
                table.instantiated_type_pack_params = instantiated_type_pack_params;
                self.arena.replace(ty, TypeKind::Table(table));
            }
            TypeKind::Metatable {
                table,
                metatable,
                name: _,
            } => {
                self.arena.replace(
                    ty,
                    TypeKind::Metatable {
                        table,
                        metatable,
                        name: Some(alias_name.to_owned()),
                    },
                );
            }
            TypeKind::TypeFunctionInstance { name, arguments }
                if let Some((table, metatable)) =
                    setmetatable_type_function_arguments(&name, &arguments)
                    && let Some((table, metatable)) =
                        self.pending_setmetatable_alias_operands(table, metatable) =>
            {
                self.arena.replace(
                    ty,
                    TypeKind::Metatable {
                        table,
                        metatable,
                        name: Some(alias_name.to_owned()),
                    },
                );
            }
            _ => {}
        }
        ty
    }

    fn alias_result_is_instantiated_self(
        &self,
        table: &TableType,
        ty: TypeId,
        instantiated_type_params: &[TypeId],
    ) -> bool {
        table.name.is_some()
            && instantiated_type_params
                .iter()
                .any(|param| self.arena.follow(*param) == ty)
    }

    fn pending_setmetatable_alias_operands(
        &self,
        table: TypeId,
        metatable: TypeId,
    ) -> Option<(TypeId, TypeId)> {
        let table = self.arena.follow(table);
        let metatable = self.arena.follow(metatable);
        matches!(
            self.arena.get(table),
            TypeKind::Table(_) | TypeKind::Metatable { .. }
        )
        .then_some(())?;
        matches!(self.arena.get(metatable), TypeKind::Blocked(_)).then_some((table, metatable))
    }

    pub(crate) fn lower_non_generic_alias(
        &mut self,
        scope: ScopeId,
        alias_name: &str,
        alias_identity: TableAliasIdentity,
        alias: &Type,
    ) -> TypeId {
        if let Some(cached) = self
            .alias_lowering
            .type_alias_cache
            .get(&alias_identity)
            .copied()
        {
            return cached;
        }
        let placeholder = self.arena.alloc(TypeKind::Blocked(BlockedType {
            reason: Some(format!("type alias {alias_name}")),
        }));
        self.alias_lowering
            .type_alias_cache
            .insert(alias_identity.clone(), placeholder);
        let ty = self.with_type_alias_definition_frame(
            alias_name.to_owned(),
            alias_identity.clone(),
            |this| this.lower_type(scope, alias),
        );
        let (ty, _) = self.reduce_alias_type_function(ty);
        if self.type_has_transparent_alias_occurrence(placeholder, ty) {
            self.report_recursive_type_alias(alias_name, alias);
            self.arena.replace(placeholder, TypeKind::Error);
            return placeholder;
        }
        let ty = self.name_type_alias_result(
            ty,
            alias_name,
            Some(alias_identity),
            Vec::new(),
            Vec::new(),
        );
        let target = self.arena.follow(ty);
        if target != placeholder
            && !self.type_contains_type(placeholder, target, &mut BTreeSet::new())
        {
            self.arena.replace(placeholder, TypeKind::Bound(target));
        } else {
            let replacement = self.arena.get(target).clone();
            self.arena.replace(placeholder, replacement);
        }
        placeholder
    }

    fn type_has_transparent_alias_occurrence(&self, placeholder: TypeId, ty: TypeId) -> bool {
        self.type_has_transparent_alias_occurrence_inner(placeholder, ty, &mut BTreeSet::new())
    }

    fn type_has_transparent_alias_occurrence_inner(
        &self,
        placeholder: TypeId,
        ty: TypeId,
        seen: &mut BTreeSet<TypeId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if ty == placeholder {
            return true;
        }
        if !seen.insert(ty) {
            return false;
        }
        match self.arena.get(ty) {
            TypeKind::Union(types) | TypeKind::Intersection(types) => types
                .iter()
                .any(|ty| self.type_has_transparent_alias_occurrence_inner(placeholder, *ty, seen)),
            TypeKind::TypeFunctionInstance { arguments, .. } => arguments
                .iter()
                .any(|ty| self.type_has_transparent_alias_occurrence_inner(placeholder, *ty, seen)),
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.type_has_transparent_alias_occurrence_inner(placeholder, *inner, seen)
            }
            TypeKind::Function(_)
            | TypeKind::Table(_)
            | TypeKind::Metatable { .. }
            | TypeKind::Extern { .. }
            | TypeKind::Primitive(_)
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

    pub(crate) fn reduce_alias_type_function(&mut self, ty: TypeId) -> (TypeId, bool) {
        let ty = self.arena.follow(ty);
        let TypeKind::TypeFunctionInstance { name, arguments } = self.arena.get(ty).clone() else {
            return (ty, false);
        };
        match TypeFunctionRuntime::new().reduce_allocating(self.arena, &name, &arguments) {
            Reduction::Reduced(reduced)
                if name == SETMETATABLE_TYPE_FUNCTION
                    && matches!(self.arena.get(self.arena.follow(reduced)), TypeKind::Never) =>
            {
                (ty, false)
            }
            Reduction::Reduced(reduced) => (self.arena.follow(reduced), true),
            Reduction::Pending => (ty, false),
        }
    }

    fn lower_type_function_arguments(
        &mut self,
        scope: ScopeId,
        name: &str,
        parameters: &[TypeParameter],
        location: Option<DiagnosticLocation>,
    ) -> Option<Vec<TypeId>> {
        let mut arguments = Vec::with_capacity(parameters.len());
        for parameter in parameters {
            match parameter {
                TypeParameter::Type(ty) => arguments.push(self.lower_type(scope, ty)),
                TypeParameter::Pack(_) => {
                    self.generated.diagnostics.push(
                        Diagnostic::error(
                            DiagnosticCategory::Generic,
                            location.unwrap_or_else(DiagnosticLocation::missing),
                        )
                        .with_typed(
                            crate::diagnostics::Payload::TypeFunctionPackArgument {
                                type_function: name.to_owned(),
                            },
                        ),
                    );
                    return None;
                }
            }
        }
        Some(arguments)
    }

    fn generic_type_substitution(&self, name: &str) -> Option<TypeId> {
        self.alias_lowering
            .generic_type_substitutions
            .iter()
            .rev()
            .find_map(|substitutions| substitutions.get(name).copied())
    }

    fn generic_type_pack_substitution(&self, name: &str) -> Option<TypePackId> {
        self.alias_lowering
            .generic_type_pack_substitutions
            .iter()
            .rev()
            .find_map(|substitutions| substitutions.get(name).copied())
    }

    pub(crate) fn function_type_generic_substitutions(
        &mut self,
        generics: &[ruau_ast::syntax::GenericType],
    ) -> (Vec<GenericType>, BTreeMap<String, TypeId>) {
        let mut function_generics = Vec::with_capacity(generics.len());
        let mut substitutions = BTreeMap::new();
        for generic in generics {
            let function_generic = GenericType {
                name: generic.name.as_str().to_owned(),
                level: TypeLevel(0),
            };
            let ty = self
                .arena
                .alloc(TypeKind::Generic(function_generic.clone()));
            substitutions.insert(function_generic.name.clone(), ty);
            function_generics.push(function_generic);
        }
        (function_generics, substitutions)
    }

    pub(crate) fn function_type_generic_pack_substitutions(
        &mut self,
        generic_packs: &[ruau_ast::syntax::GenericTypePack],
    ) -> (Vec<GenericTypePack>, BTreeMap<String, TypePackId>) {
        let mut function_generic_packs = Vec::with_capacity(generic_packs.len());
        let mut substitutions = BTreeMap::new();
        for generic_pack in generic_packs {
            let function_generic_pack = GenericTypePack {
                name: generic_pack.name.as_str().to_owned(),
                level: TypeLevel(0),
            };
            let pack = self
                .arena
                .alloc_pack(TypePackKind::Generic(function_generic_pack.clone()));
            substitutions.insert(function_generic_pack.name.clone(), pack);
            function_generic_packs.push(function_generic_pack);
        }
        (function_generic_packs, substitutions)
    }

    #[allow(clippy::too_many_arguments)]
    fn generic_alias_substitutions(
        &mut self,
        scope: ScopeId,
        alias_name: &str,
        generics: &AliasGenerics<'_>,
        parameters: &[TypeParameter],
        has_parameter_list: bool,
        location: Option<DiagnosticLocation>,
        allow_generic_type_arguments: bool,
    ) -> Option<GenericAliasSubstitutions> {
        let allow_generic_type_arguments = allow_generic_type_arguments
            || self.alias_lowering.type_alias_function_depth > 0
            || self.alias_lowering.generic_alias_type_argument_depth > 0;
        let mut types = BTreeMap::new();
        let mut packs = BTreeMap::new();
        let mut instantiated_type_params = Vec::new();
        let mut instantiated_pack_params = Vec::new();
        let mut parameter_index = 0;
        if parameters.is_empty()
            && !has_parameter_list
            && (generics.defaults.iter().any(Option::is_none)
                || generics.pack_defaults.iter().any(Option::is_none))
        {
            self.report_generic_alias_parameter_count(
                alias_name,
                generics.names.len(),
                generics.pack_names.len(),
                parameters,
                location,
            );
            return None;
        }
        if generic_alias::arguments_are_out_of_order(parameters, generics.names.len()) {
            self.report_generic_alias_parameter_order(alias_name, location);
            return None;
        }
        for (index, generic_name) in generics.names.iter().enumerate() {
            let ty = match parameters.get(parameter_index) {
                Some(TypeParameter::Type(ty))
                    if self.generic_alias_argument_is_instantiable_syntax(scope, ty) =>
                {
                    let ty = self.lower_type(scope, ty);
                    if !self.generic_alias_argument_is_concrete(ty, allow_generic_type_arguments) {
                        return None;
                    }
                    ty
                }
                Some(TypeParameter::Type(_)) => return None,
                Some(TypeParameter::Pack(_)) => {
                    if generics.defaults[index..]
                        .iter()
                        .all(std::option::Option::is_some)
                    {
                        self.report_generic_alias_parameter_order(alias_name, location);
                    } else {
                        self.report_generic_alias_parameter_count(
                            alias_name,
                            generics.names.len(),
                            generics.pack_names.len(),
                            parameters,
                            location,
                        );
                    }
                    return None;
                }
                None => {
                    let Some(default) = generics.defaults.get(index).and_then(Option::as_ref)
                    else {
                        self.report_generic_alias_parameter_count(
                            alias_name,
                            generics.names.len(),
                            generics.pack_names.len(),
                            parameters,
                            location,
                        );
                        return None;
                    };
                    self.lower_generic_alias_default_type(
                        scope,
                        default,
                        generics.names,
                        generics.pack_names,
                        &types,
                        &packs,
                        location,
                    )?
                }
            };
            types.insert(generic_name.clone(), ty);
            instantiated_type_params.push(ty);
            if matches!(
                parameters.get(parameter_index),
                Some(TypeParameter::Type(_))
            ) {
                parameter_index += 1;
            }
        }
        for (index, generic_pack_name) in generics.pack_names.iter().enumerate() {
            let pack = match parameters.get(parameter_index) {
                Some(TypeParameter::Pack(pack)) => self.lower_type_pack(scope, pack),
                Some(TypeParameter::Type(ty))
                    if generic_alias::type_argument_can_follow_pack(ty) =>
                {
                    let ty = self.lower_type(scope, ty);
                    parameter_index += 1;
                    self.arena.alloc_pack(TypePackKind::List {
                        types: vec![ty],
                        tail: None,
                    })
                }
                Some(TypeParameter::Type(_)) => {
                    if index == 0 {
                        let start = parameter_index;
                        while matches!(
                            parameters.get(parameter_index),
                            Some(TypeParameter::Type(_))
                        ) {
                            parameter_index += 1;
                        }
                        self.lower_generic_alias_type_arguments_as_pack(
                            scope,
                            &parameters[start..parameter_index],
                        )?
                    } else {
                        self.report_generic_alias_parameter_count(
                            alias_name,
                            generics.names.len(),
                            generics.pack_names.len(),
                            parameters,
                            location,
                        );
                        return None;
                    }
                }
                None => {
                    if let Some(default) =
                        generics.pack_defaults.get(index).and_then(Option::as_ref)
                    {
                        self.lower_generic_alias_default_type_pack(
                            scope,
                            default,
                            generics.names,
                            generics.pack_names,
                            &types,
                            &packs,
                            location,
                        )?
                    } else {
                        if generics.pack_names.len() != 1 {
                            self.report_generic_alias_parameter_count(
                                alias_name,
                                generics.names.len(),
                                generics.pack_names.len(),
                                parameters,
                                location,
                            );
                            return None;
                        }
                        self.arena.alloc_pack(TypePackKind::List {
                            types: Vec::new(),
                            tail: None,
                        })
                    }
                }
            };
            packs.insert(generic_pack_name.clone(), pack);
            instantiated_pack_params.push(pack);
            if matches!(
                parameters.get(parameter_index),
                Some(TypeParameter::Pack(_))
            ) {
                parameter_index += 1;
            }
        }
        if parameter_index < parameters.len() {
            self.report_generic_alias_parameter_count(
                alias_name,
                generics.names.len(),
                generics.pack_names.len(),
                parameters,
                location,
            );
            return None;
        }
        Some(GenericAliasSubstitutions {
            types,
            packs,
            instantiated_type_params,
            instantiated_pack_params,
        })
    }

    fn lower_generic_alias_type_arguments_as_pack(
        &mut self,
        scope: ScopeId,
        parameters: &[TypeParameter],
    ) -> Option<TypePackId> {
        let mut types = Vec::with_capacity(parameters.len());
        for parameter in parameters {
            let TypeParameter::Type(ty) = parameter else {
                return None;
            };
            types.push(self.lower_type(scope, ty));
        }
        Some(
            self.arena
                .alloc_pack(TypePackKind::List { types, tail: None }),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_generic_alias_default_type(
        &mut self,
        scope: ScopeId,
        default: &Type,
        generic_names: &[String],
        generic_pack_names: &[String],
        types: &BTreeMap<String, TypeId>,
        packs: &BTreeMap<String, TypePackId>,
        location: Option<DiagnosticLocation>,
    ) -> Option<TypeId> {
        if let Some(name) = self.unavailable_generic_in_default_type(
            default,
            generic_names,
            generic_pack_names,
            types,
            packs,
        ) {
            self.report_unknown_generic_default(name, location);
            return None;
        }
        let ty = self.with_generic_type_substitution_frame(types.clone(), packs.clone(), |this| {
            this.lower_type(scope, default)
        });
        Some(ty)
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_generic_alias_default_type_pack(
        &mut self,
        scope: ScopeId,
        default: &TypePack,
        generic_names: &[String],
        generic_pack_names: &[String],
        types: &BTreeMap<String, TypeId>,
        packs: &BTreeMap<String, TypePackId>,
        location: Option<DiagnosticLocation>,
    ) -> Option<TypePackId> {
        if let Some(name) = self.unavailable_generic_in_default_type_pack(
            default,
            generic_names,
            generic_pack_names,
            types,
            packs,
        ) {
            self.report_unknown_generic_default(name, location);
            return None;
        }
        let pack =
            self.with_generic_type_substitution_frame(types.clone(), packs.clone(), |this| {
                this.lower_type_pack(scope, default)
            });
        Some(pack)
    }

    fn report_generic_pack_used_as_type(
        &mut self,
        name: &str,
        location: Option<DiagnosticLocation>,
    ) {
        self.generated
            .diagnostics
            .push(generic_alias::generic_pack_used_as_type_diagnostic(
                name,
                location.unwrap_or_else(DiagnosticLocation::missing),
            ));
    }

    fn report_recursive_type_alias(&mut self, alias_name: &str, alias: &Type) {
        self.generated
            .diagnostics
            .push(generic_alias::recursive_type_alias_diagnostic(
                alias_name,
                type_location(alias),
            ));
    }

    fn report_generic_type_used_as_pack(
        &mut self,
        name: &str,
        location: Option<DiagnosticLocation>,
    ) {
        self.generated
            .diagnostics
            .push(generic_alias::generic_type_used_as_pack_diagnostic(
                name,
                location.unwrap_or_else(DiagnosticLocation::missing),
            ));
    }

    fn report_unapplied_type_function(&mut self, name: &str, location: Option<DiagnosticLocation>) {
        self.generated.diagnostics.push(
            Diagnostic::error(
                DiagnosticCategory::Generic,
                location.unwrap_or_else(DiagnosticLocation::missing),
            )
            .with_typed(crate::diagnostics::Payload::UnappliedTypeFunction {
                type_function: name.to_owned(),
            }),
        );
    }

    fn report_generic_alias_parameter_count(
        &mut self,
        alias_name: &str,
        expected_types: usize,
        expected_packs: usize,
        parameters: &[TypeParameter],
        location: Option<DiagnosticLocation>,
    ) {
        let actual_types = parameters
            .iter()
            .filter(|parameter| matches!(parameter, TypeParameter::Type(_)))
            .count();
        let actual_packs = parameters.len() - actual_types;
        self.generated.diagnostics.push(
            Diagnostic::error(
                DiagnosticCategory::Generic,
                location.unwrap_or_else(DiagnosticLocation::missing),
            )
            .with_typed(crate::diagnostics::Payload::GenericAliasParameterCount {
                alias: alias_name.to_owned(),
                expected_type_parameters: expected_types,
                expected_type_pack_parameters: expected_packs,
                actual_type_parameters: actual_types,
                actual_type_pack_parameters: actual_packs,
            }),
        );
    }

    fn report_generic_alias_parameter_order(
        &mut self,
        alias_name: &str,
        location: Option<DiagnosticLocation>,
    ) {
        self.generated.diagnostics.push(
            Diagnostic::error(
                DiagnosticCategory::Generic,
                location.unwrap_or_else(DiagnosticLocation::missing),
            )
            .with_context("Type parameters must come before type pack parameters")
            .with_typed(crate::diagnostics::Payload::GenericAliasParameterOrder {
                alias: alias_name.to_owned(),
            }),
        );
    }

    fn report_unknown_generic_default(
        &mut self,
        name: String,
        location: Option<DiagnosticLocation>,
    ) {
        self.generated.diagnostics.push(Diagnostic::unknown_type(
            name,
            location.unwrap_or_else(DiagnosticLocation::missing),
        ));
    }

    fn unavailable_generic_in_default_type(
        &self,
        ty: &Type,
        generic_names: &[String],
        generic_pack_names: &[String],
        types: &BTreeMap<String, TypeId>,
        packs: &BTreeMap<String, TypePackId>,
    ) -> Option<String> {
        match ty {
            Type::Reference {
                prefix,
                name,
                parameters,
                ..
            } => {
                let name = name.as_str();
                if prefix.is_none()
                    && parameters.is_empty()
                    && generic_names.iter().any(|generic| generic == name)
                    && !types.contains_key(name)
                {
                    return Some(name.to_owned());
                }
                parameters.iter().find_map(|parameter| match parameter {
                    TypeParameter::Type(ty) => self.unavailable_generic_in_default_type(
                        ty,
                        generic_names,
                        generic_pack_names,
                        types,
                        packs,
                    ),
                    TypeParameter::Pack(pack) => self.unavailable_generic_in_default_type_pack(
                        pack,
                        generic_names,
                        generic_pack_names,
                        types,
                        packs,
                    ),
                })
            }
            Type::Group { inner, .. } => self.unavailable_generic_in_default_type(
                inner,
                generic_names,
                generic_pack_names,
                types,
                packs,
            ),
            Type::Union { types: members, .. } | Type::Intersection { types: members, .. } => {
                members.iter().find_map(|ty| {
                    self.unavailable_generic_in_default_type(
                        ty,
                        generic_names,
                        generic_pack_names,
                        types,
                        packs,
                    )
                })
            }
            Type::Function {
                arg_types,
                return_types,
                ..
            } => arg_types
                .types
                .iter()
                .find_map(|ty| {
                    self.unavailable_generic_in_default_type(
                        ty,
                        generic_names,
                        generic_pack_names,
                        types,
                        packs,
                    )
                })
                .or_else(|| {
                    arg_types.tail_type.as_deref().and_then(|pack| {
                        self.unavailable_generic_in_default_type_pack(
                            pack,
                            generic_names,
                            generic_pack_names,
                            types,
                            packs,
                        )
                    })
                })
                .or_else(|| {
                    self.unavailable_generic_in_default_type_pack(
                        return_types,
                        generic_names,
                        generic_pack_names,
                        types,
                        packs,
                    )
                }),
            Type::Table { props, indexer, .. } => props
                .iter()
                .find_map(|prop| {
                    self.unavailable_generic_in_default_type(
                        &prop.prop_type,
                        generic_names,
                        generic_pack_names,
                        types,
                        packs,
                    )
                })
                .or_else(|| {
                    indexer.as_ref().and_then(|indexer| {
                        self.unavailable_generic_in_default_type(
                            &indexer.index_type,
                            generic_names,
                            generic_pack_names,
                            types,
                            packs,
                        )
                        .or_else(|| {
                            self.unavailable_generic_in_default_type(
                                &indexer.result_type,
                                generic_names,
                                generic_pack_names,
                                types,
                                packs,
                            )
                        })
                    })
                }),
            Type::Error { types: members, .. } => members.iter().find_map(|ty| {
                self.unavailable_generic_in_default_type(
                    ty,
                    generic_names,
                    generic_pack_names,
                    types,
                    packs,
                )
            }),
            Type::SingletonString { .. }
            | Type::SingletonBool { .. }
            | Type::Optional { .. }
            | Type::Typeof { .. } => None,
        }
    }

    fn unavailable_generic_in_default_type_pack(
        &self,
        pack: &TypePack,
        generic_names: &[String],
        generic_pack_names: &[String],
        types: &BTreeMap<String, TypeId>,
        packs: &BTreeMap<String, TypePackId>,
    ) -> Option<String> {
        match pack {
            TypePack::Explicit { type_list, .. } => type_list
                .types
                .iter()
                .find_map(|ty| {
                    self.unavailable_generic_in_default_type(
                        ty,
                        generic_names,
                        generic_pack_names,
                        types,
                        packs,
                    )
                })
                .or_else(|| {
                    type_list.tail_type.as_deref().and_then(|tail| {
                        self.unavailable_generic_in_default_type_pack(
                            tail,
                            generic_names,
                            generic_pack_names,
                            types,
                            packs,
                        )
                    })
                }),
            TypePack::Variadic { variadic_type, .. } => self.unavailable_generic_in_default_type(
                variadic_type,
                generic_names,
                generic_pack_names,
                types,
                packs,
            ),
            TypePack::Generic { name, .. } => {
                let name = name.as_str();
                if generic_pack_names.iter().any(|generic| generic == name)
                    && !packs.contains_key(name)
                {
                    Some(name.to_owned())
                } else {
                    None
                }
            }
        }
    }

    fn generic_alias_argument_is_instantiable_syntax(&self, scope: ScopeId, ty: &Type) -> bool {
        match ty {
            Type::Reference {
                location,
                prefix,
                name,
                name_location,
                parameters,
                ..
            } => {
                if prefix.is_none()
                    && parameters.is_empty()
                    && self.generic_type_substitution(name.as_str()).is_some()
                {
                    return true;
                }
                let qualified_name = prefix
                    .as_ref()
                    .map(|prefix| format!("{}.{}", prefix.as_str(), name.as_str()));
                let lookup_name = qualified_name.as_deref().unwrap_or(name.as_str());
                let Some((_, binding)) =
                    self.input.scopes.lookup_type_with_scope(scope, lookup_name)
                else {
                    return false;
                };
                if binding.alias_has_generics
                    && parameters.is_empty()
                    && !generic_alias::type_reference_has_parameter_list(*location, *name_location)
                {
                    return false;
                }
                !binding.alias_has_generics
                    || parameters.iter().all(|parameter| match parameter {
                        TypeParameter::Type(ty) => {
                            self.generic_alias_argument_is_instantiable_syntax(scope, ty)
                        }
                        TypeParameter::Pack(_) => false,
                    })
            }
            Type::Group { inner, .. } => {
                self.generic_alias_argument_is_instantiable_syntax(scope, inner)
            }
            Type::Union { types, .. } | Type::Intersection { types, .. } => types
                .iter()
                .all(|ty| self.generic_alias_argument_is_instantiable_syntax(scope, ty)),
            Type::Optional { .. }
            | Type::Function { .. }
            | Type::Table { .. }
            | Type::Typeof { .. }
            | Type::SingletonString { .. }
            | Type::SingletonBool { .. } => true,
            Type::Error { .. } => false,
        }
    }

    fn generic_alias_argument_is_concrete(
        &self,
        ty: TypeId,
        allow_generic_type_arguments: bool,
    ) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            // While materializing an alias, a Blocked type is a recursive
            // type-alias placeholder (the seed both alias-lowering paths install
            // before lowering the body). That is a legitimate equirecursive
            // argument — e.g. `Wrapped` in `type Wrapped = Table<Wrapped>` — so
            // it instantiates the generic alias rather than collapsing to `any`.
            TypeKind::Blocked(_) if !self.alias_lowering.type_alias_stack.is_empty() => true,
            TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Error
            | TypeKind::Free(_)
            | TypeKind::Blocked(_) => false,
            TypeKind::Generic(_) => allow_generic_type_arguments,
            TypeKind::Union(types) | TypeKind::Intersection(types) => types.iter().all(|ty| {
                self.generic_alias_argument_is_concrete(*ty, allow_generic_type_arguments)
            }),
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => {
                self.generic_alias_argument_is_concrete(*inner, allow_generic_type_arguments)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Function(_)
            | TypeKind::Table(_)
            | TypeKind::Metatable { .. }
            | TypeKind::Extern { .. }
            | TypeKind::TypeFunctionInstance { .. }
            | TypeKind::Never => true,
        }
    }

    fn generic_alias_allows_recursive_generic_type_arguments(
        alias_name: &str,
        generic_pack_names: &[String],
        alias: &Type,
    ) -> bool {
        generic_pack_names.is_empty() && Self::type_mentions_alias(alias, alias_name)
    }

    fn type_mentions_alias(ty: &Type, alias_name: &str) -> bool {
        match ty {
            Type::Reference {
                prefix,
                name,
                parameters,
                ..
            } => {
                let qualified_name = prefix
                    .as_ref()
                    .map(|prefix| format!("{}.{}", prefix.as_str(), name.as_str()));
                qualified_name.as_deref().unwrap_or(name.as_str()) == alias_name
                    || parameters
                        .iter()
                        .any(|parameter| Self::type_parameter_mentions_alias(parameter, alias_name))
            }
            Type::Group { inner, .. } => Self::type_mentions_alias(inner, alias_name),
            Type::Union { types, .. } | Type::Intersection { types, .. } => types
                .iter()
                .any(|ty| Self::type_mentions_alias(ty, alias_name)),
            Type::Function {
                arg_types,
                return_types,
                ..
            } => {
                Self::type_list_mentions_alias(arg_types, alias_name)
                    || Self::type_pack_mentions_alias(return_types, alias_name)
            }
            Type::Table { props, indexer, .. } => {
                props
                    .iter()
                    .any(|prop| Self::type_mentions_alias(&prop.prop_type, alias_name))
                    || indexer.as_ref().is_some_and(|indexer| {
                        Self::type_mentions_alias(&indexer.index_type, alias_name)
                            || Self::type_mentions_alias(&indexer.result_type, alias_name)
                    })
            }
            Type::Error { types, .. } => types
                .iter()
                .any(|ty| Self::type_mentions_alias(ty, alias_name)),
            Type::Typeof { .. }
            | Type::Optional { .. }
            | Type::SingletonString { .. }
            | Type::SingletonBool { .. } => false,
        }
    }

    fn type_list_mentions_alias(list: &TypeList, alias_name: &str) -> bool {
        list.types
            .iter()
            .any(|ty| Self::type_mentions_alias(ty, alias_name))
            || list
                .tail_type
                .as_ref()
                .is_some_and(|tail| Self::type_pack_mentions_alias(tail, alias_name))
    }

    fn type_pack_mentions_alias(pack: &TypePack, alias_name: &str) -> bool {
        match pack {
            TypePack::Explicit { type_list, .. } => {
                Self::type_list_mentions_alias(type_list, alias_name)
            }
            TypePack::Variadic { variadic_type, .. } => {
                Self::type_mentions_alias(variadic_type, alias_name)
            }
            TypePack::Generic { .. } => false,
        }
    }

    fn type_parameter_mentions_alias(parameter: &TypeParameter, alias_name: &str) -> bool {
        match parameter {
            TypeParameter::Type(ty) => Self::type_mentions_alias(ty, alias_name),
            TypeParameter::Pack(pack) => Self::type_pack_mentions_alias(pack, alias_name),
        }
    }

    pub(crate) fn lower_type_list(&mut self, scope: ScopeId, list: &TypeList) -> TypePackId {
        self.lower_type_list_with_context(scope, list, TypePackLoweringContext::TypeAnnotation)
    }

    fn lower_type_list_with_context(
        &mut self,
        scope: ScopeId,
        list: &TypeList,
        context: TypePackLoweringContext,
    ) -> TypePackId {
        let types = list
            .types
            .iter()
            .map(|ty| self.lower_type(scope, ty))
            .collect::<Vec<_>>();
        let tail = list
            .tail_type
            .as_deref()
            .map(|tail| self.lower_type_pack_with_context(scope, tail, context));
        // A generic-pack tail substituted with a concrete pack (`(T...)` with
        // `T... = (number)`) leaves a list in tail position; splice it so the
        // annotation lowers to the same shape as writing the types directly.
        let (types, tail) = self.arena.flatten_list_pack_parts(types, tail);
        self.arena.alloc_pack(TypePackKind::List { types, tail })
    }

    pub(crate) fn lower_class_binding(
        &mut self,
        scope: ScopeId,
        name: &str,
        super_name: &Option<String>,
        props: Vec<DeclaredClassProp>,
        indexer: Option<AstTableIndexer>,
    ) -> TypeId {
        let key = (scope, name.to_owned());
        let placeholder = self
            .alias_lowering
            .class_lowering_placeholders
            .get(&key)
            .copied()
            .unwrap_or_else(|| {
                let ty = self.empty_extern_type(name, &[]);
                self.alias_lowering
                    .class_lowering_placeholders
                    .insert(key.clone(), ty);
                ty
            });
        let parents = self.declared_class_parent_names(scope, super_name.as_deref());
        let mut properties = self.inherited_class_properties(scope, super_name.as_deref());
        let inherited_property_names = properties.keys().cloned().collect::<BTreeSet<_>>();
        let indexer = indexer
            .map(|indexer| TableIndexer {
                key: self.lower_type(scope, &indexer.index_type),
                value: self.lower_type(scope, &indexer.result_type),
                read_only: indexer.read_only,
            })
            .or_else(|| self.inherited_class_indexer(scope, super_name.as_deref()));
        for prop in props {
            let mut ty = self.lower_type(scope, &prop.declared_type);
            if prop.is_method {
                ty = self.declared_method_property_type(name, &parents, ty);
            }
            let prop_name = prop.name.as_str();
            let documentation_symbol = format!("@test/globaltype/{name}.{prop_name}");
            if let Some(existing) = properties.get_mut(prop_name) {
                let overrides_inherited = inherited_property_names.contains(prop_name);
                if prop.read_only {
                    if existing.write_only {
                        existing.write_ty = Some(existing.ty);
                        existing.write_only = false;
                        existing.ty = ty;
                        existing.read_only = false;
                    } else {
                        existing.ty = if overrides_inherited {
                            ty
                        } else {
                            self.intersection_type(vec![existing.ty, ty])
                        };
                        existing.read_only = existing.write_ty.is_none();
                    }
                } else if prop.write_only {
                    existing.write_ty = Some(if overrides_inherited || existing.read_only {
                        ty
                    } else {
                        self.intersection_type(vec![existing.write_type(), ty])
                    });
                    if existing.read_only {
                        existing.read_only = false;
                    }
                    existing.write_only = false;
                } else {
                    existing.ty = if overrides_inherited {
                        ty
                    } else {
                        self.intersection_type(vec![existing.ty, ty])
                    };
                    existing.write_ty = None;
                    existing.read_only = false;
                    existing.write_only = false;
                }
                existing
                    .documentation_symbol
                    .get_or_insert(documentation_symbol);
            } else {
                let mut property =
                    TableProperty::new(ty).with_documentation_symbol(documentation_symbol);
                property.read_only = prop.read_only;
                property.write_only = prop.write_only;
                properties.insert(prop_name.to_owned(), property);
            }
        }

        self.arena.replace(
            placeholder,
            TypeKind::Extern {
                name: name.to_owned(),
                parents,
                properties,
                indexer,
            },
        );
        placeholder
    }

    fn empty_extern_type(&mut self, name: &str, parents: &[String]) -> TypeId {
        self.arena.alloc(TypeKind::Extern {
            name: name.to_owned(),
            parents: parents.to_vec(),
            properties: BTreeMap::new(),
            indexer: None,
        })
    }

    fn declared_method_property_type(
        &mut self,
        class_name: &str,
        parents: &[String],
        ty: TypeId,
    ) -> TypeId {
        let TypeKind::Function(mut function) = self.arena.get(self.arena.follow(ty)).clone() else {
            return ty;
        };
        let mut argument_pack = self.arena.normalize_pack(function.arguments);
        let self_ty = self.arena.alloc(TypeKind::Extern {
            name: class_name.to_owned(),
            parents: parents.to_vec(),
            properties: BTreeMap::new(),
            indexer: None,
        });
        argument_pack.types.insert(0, self_ty);
        let tail = self.arena.alloc_optional_pack_tail(argument_pack.tail);
        function.arguments = self.arena.alloc_pack(TypePackKind::List {
            types: argument_pack.types,
            tail,
        });
        function.has_self = true;
        self.arena.alloc(TypeKind::Function(function))
    }

    fn inherited_class_properties(
        &mut self,
        scope: ScopeId,
        super_name: Option<&str>,
    ) -> BTreeMap<String, TableProperty> {
        let Some(super_name) = super_name else {
            return BTreeMap::new();
        };
        if self
            .alias_lowering
            .type_alias_stack
            .iter()
            .any(|alias| alias == super_name)
        {
            return BTreeMap::new();
        }
        let Some((super_scope, super_binding)) =
            self.input.scopes.lookup_type_with_scope(scope, super_name)
        else {
            return BTreeMap::new();
        };
        if !matches!(
            super_binding.kind,
            TypeBindingKind::Class | TypeBindingKind::DeclaredClass
        ) {
            return BTreeMap::new();
        }
        let super_super_name = super_binding.class_super_name.clone();
        let super_props = super_binding.class_props.clone();
        let super_indexer = super_binding.class_indexer.clone();
        let super_ty = self.with_type_alias_frame(super_name.to_owned(), |this| {
            this.lower_class_binding(
                super_scope,
                super_name,
                &super_super_name,
                super_props,
                super_indexer,
            )
        });

        let TypeKind::Extern { properties, .. } =
            self.arena.get(self.arena.follow(super_ty)).clone()
        else {
            return BTreeMap::new();
        };
        properties
    }

    fn inherited_class_indexer(
        &mut self,
        scope: ScopeId,
        super_name: Option<&str>,
    ) -> Option<TableIndexer> {
        let super_name = super_name?;
        if self
            .alias_lowering
            .type_alias_stack
            .iter()
            .any(|alias| alias == super_name)
        {
            return None;
        }
        let (super_scope, super_binding) = self
            .input
            .scopes
            .lookup_type_with_scope(scope, super_name)?;
        if !matches!(
            super_binding.kind,
            TypeBindingKind::Class | TypeBindingKind::DeclaredClass
        ) {
            return None;
        }
        let super_super_name = super_binding.class_super_name.clone();
        let super_props = super_binding.class_props.clone();
        let super_indexer = super_binding.class_indexer.clone();
        let super_ty = self.with_type_alias_frame(super_name.to_owned(), |this| {
            this.lower_class_binding(
                super_scope,
                super_name,
                &super_super_name,
                super_props,
                super_indexer,
            )
        });

        let TypeKind::Extern { indexer, .. } = self.arena.get(self.arena.follow(super_ty)).clone()
        else {
            return None;
        };
        indexer
    }

    pub(crate) fn attach_table_property_documentation(&mut self, ty: TypeId, base_symbol: &str) {
        let ty = self.arena.follow(ty);
        let TypeKind::Table(mut table) = self.arena.get(ty).clone() else {
            return;
        };
        for (name, property) in &mut table.properties {
            property
                .documentation_symbol
                .get_or_insert_with(|| format!("{base_symbol}.{name}"));
        }
        self.arena.replace(ty, TypeKind::Table(table));
    }

    fn declared_class_parent_names(&self, scope: ScopeId, super_name: Option<&str>) -> Vec<String> {
        let mut parents = Vec::new();
        let mut seen = BTreeSet::new();
        self.collect_declared_class_parent_names(scope, super_name, &mut parents, &mut seen);
        parents
    }

    fn collect_declared_class_parent_names(
        &self,
        scope: ScopeId,
        super_name: Option<&str>,
        parents: &mut Vec<String>,
        seen: &mut BTreeSet<String>,
    ) {
        let Some(super_name) = super_name else {
            return;
        };
        if !seen.insert(super_name.to_owned()) {
            return;
        }
        parents.push(super_name.to_owned());
        let Some((parent_scope, parent)) =
            self.input.scopes.lookup_type_with_scope(scope, super_name)
        else {
            return;
        };
        if !matches!(
            parent.kind,
            TypeBindingKind::Class | TypeBindingKind::DeclaredClass
        ) {
            return;
        }
        self.collect_declared_class_parent_names(
            parent_scope,
            parent.class_super_name.as_deref(),
            parents,
            seen,
        );
    }

    pub(crate) fn lower_vararg_type_pack_option(
        &mut self,
        scope: ScopeId,
        pack: Option<&TypePack>,
    ) -> TypePackId {
        if let Some(pack) = pack {
            self.lower_type_pack_with_context(
                scope,
                pack,
                TypePackLoweringContext::FunctionVarargAnnotation,
            )
        } else {
            self.arena.alloc_pack(TypePackKind::Variadic {
                ty: self.primitives().any,
            })
        }
    }
    pub(crate) fn lower_type_pack(&mut self, scope: ScopeId, pack: &TypePack) -> TypePackId {
        self.lower_type_pack_with_context(scope, pack, TypePackLoweringContext::TypeAnnotation)
    }

    fn lower_type_pack_with_context(
        &mut self,
        scope: ScopeId,
        pack: &TypePack,
        context: TypePackLoweringContext,
    ) -> TypePackId {
        match pack {
            TypePack::Explicit { type_list, .. } => {
                self.lower_type_list_with_context(scope, type_list, context)
            }
            TypePack::Variadic { variadic_type, .. } => {
                let ty = self.lower_type(scope, variadic_type);
                self.arena.alloc_pack(TypePackKind::Variadic { ty })
            }
            TypePack::Generic { location, name, .. } => {
                let name = name.as_str();
                if context == TypePackLoweringContext::FunctionVarargAnnotation
                    && let Some((_, binding)) =
                        self.input.scopes.lookup_type_with_scope(scope, name)
                    && binding.kind == TypeBindingKind::GenericParameter
                {
                    self.report_generic_type_used_as_pack(
                        name,
                        location.as_ref().copied().map(DiagnosticLocation::from),
                    );
                    return self.arena.alloc_pack(TypePackKind::Error);
                }
                if let Some(pack) = self.generic_type_pack_substitution(name) {
                    return pack;
                }
                if let Some((binding_scope, binding)) =
                    self.input.scopes.lookup_type_with_scope(scope, name)
                    && binding.kind == TypeBindingKind::GenericPackParameter
                {
                    let key = (binding_scope, name.to_owned());
                    if let Some(existing) = self.alias_lowering.generic_type_pack_cache.get(&key) {
                        return *existing;
                    }
                    let pack = self
                        .arena
                        .alloc_pack(TypePackKind::Generic(GenericTypePack {
                            name: name.to_owned(),
                            level: TypeLevel(0),
                        }));
                    self.alias_lowering
                        .generic_type_pack_cache
                        .insert(key, pack);
                    return pack;
                }
                self.arena.alloc_pack(TypePackKind::Free {
                    level: TypeLevel(0),
                    name: Some(name.to_owned()),
                })
            }
        }
    }
    pub(crate) fn pack(&mut self, types: Vec<TypeId>) -> TypePackId {
        self.arena
            .alloc_pack(TypePackKind::List { types, tail: None })
    }
    pub(crate) fn pack_with_tail(
        &mut self,
        types: Vec<TypeId>,
        tail: Option<TypePackId>,
    ) -> TypePackId {
        self.arena.alloc_pack(TypePackKind::List { types, tail })
    }
    pub(crate) fn recovery_type_at(
        &mut self,
        location: Option<Location>,
        reason: impl Into<String>,
    ) -> TypeId {
        self.generated.diagnostics.push(
            Diagnostic::error(
                DiagnosticCategory::Internal,
                location.map_or_else(DiagnosticLocation::missing, DiagnosticLocation::from),
            )
            .with_context(reason.into()),
        );
        self.primitives().error
    }
    pub(crate) fn is_error_type(&self, ty: TypeId) -> bool {
        matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Error)
    }
    pub(crate) fn is_any_type(&self, ty: TypeId) -> bool {
        matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Any)
    }
    pub(crate) fn is_dynamic(&self, mut ty: TypeId) -> bool {
        let mut seen = BTreeSet::new();
        while seen.insert(ty) {
            match self.arena.get(ty) {
                TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {
                    return true;
                }
                TypeKind::Bound(bound) => ty = *bound,
                TypeKind::Union(types) | TypeKind::Intersection(types) => {
                    return types.iter().any(|ty| self.is_dynamic(*ty));
                }
                _ => return false,
            }
        }
        false
    }
    /// Like [`Self::is_dynamic`], but treats `unknown` as a concrete type. Used
    /// on the source side of an annotated assignment: assigning an `unknown`
    /// value to a narrower annotation is a real subtype error, whereas
    /// `any`/`error`/`blocked` sources stay suppressed to avoid cascades.
    pub(crate) fn is_dynamic_assignment_source(&self, mut ty: TypeId) -> bool {
        let mut seen = BTreeSet::new();
        while seen.insert(ty) {
            match self.arena.get(ty) {
                TypeKind::Any | TypeKind::Error | TypeKind::Blocked(_) => return true,
                TypeKind::Unknown => return false,
                TypeKind::Bound(bound) => ty = *bound,
                TypeKind::Union(types) | TypeKind::Intersection(types) => {
                    return types
                        .iter()
                        .any(|ty| self.is_dynamic_assignment_source(*ty));
                }
                _ => return false,
            }
        }
        false
    }
    pub(crate) fn is_never_type(&self, ty: TypeId) -> bool {
        matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Never)
    }
    pub(crate) fn report_nilable_type_mismatch(
        &mut self,
        ty: TypeId,
        location: Option<ruau_ast::Location>,
    ) -> bool {
        if !self.arena.may_be_nil(ty) || self.is_dynamic(ty) {
            return false;
        }
        self.generated.diagnostics.push(Diagnostic::error(
            DiagnosticCategory::TypeMismatch,
            DiagnosticLocation::from_opt(location),
        ));
        true
    }
    pub(crate) fn check_nilable_callee(
        &mut self,
        callee: TypeId,
        location: Option<ruau_ast::Location>,
    ) {
        self.report_nilable_type_mismatch(callee, location);
    }
    pub(crate) fn strip_nil(&mut self, ty: TypeId) -> TypeId {
        let followed = self.arena.follow(ty);
        let TypeKind::Union(options) = self.arena.get(followed).clone() else {
            return ty;
        };
        let kept: Vec<TypeId> = options
            .into_iter()
            .filter(|option| !self.arena.is_nil(*option))
            .collect();
        if kept.is_empty() {
            return ty;
        }
        if kept.len() == 1 {
            return kept[0];
        }
        self.union_type(kept)
    }
}
