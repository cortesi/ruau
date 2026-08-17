//! Table- and function-literal type inference for expression constraint
//! generation.
//!
//! Owns `expr_table` and `expr_function`: inferring a literal table type from
//! its items against any expected shape, and inferring a function literal's
//! signature (parameters, contextual parameter types, generics, and body).

use std::collections::{BTreeMap, BTreeSet};

use ruau_syntax::{
    Expr, Local, LocalId, Location, Stat, TableItem, TableItemKind, Type, TypeList, TypePack,
};

use crate::{
    call_pack::{ExpectedCallParameterPack, ExpectedFunctionReturnPack, ReceiverParameter},
    constraints::{Constraint, ConstraintSolveError},
    diagnostics::{Diagnostic, DiagnosticCategory, DiagnosticLocation},
    generation::{
        expression::{
            expected_table_item, is_operator_metamethod_name, shadowed_table_item_indices,
            widened_table_literal_value_type,
        },
        state::{ExpressionConstraintGenerator, InferredReturnPath, ParameterExpectations},
    },
    graph::Mode,
    normalize::simplify_type,
    scopes::ScopeId,
    subtype::Subtyper,
    types::{
        FunctionType, GenericType, GenericTypePack, SingletonType, TableIndexer, TableProperty,
        TableState, TableType, TypeId, TypeKind, TypeLevel, TypePackId, TypePackKind,
    },
};

struct FunctionFrame {
    return_pack: TypePackId,
    vararg_pack: Option<TypePackId>,
    parameter_expectations: BTreeMap<LocalId, ParameterExpectations>,
    unannotated_return: bool,
    contextual_return: bool,
    local_function_id: Option<LocalId>,
    global_function_name: Option<String>,
    has_unannotated_parameter: bool,
    recursive_return_placeholder: Option<TypeId>,
    function_scope: ScopeId,
    function_is_global: bool,
}

fn type_list_may_contain_type_function(list: &TypeList) -> bool {
    list.types.iter().any(annotation_may_contain_type_function)
        || list
            .tail_type
            .as_deref()
            .is_some_and(type_pack_may_contain_type_function)
}

fn type_pack_may_contain_type_function(pack: &TypePack) -> bool {
    match pack {
        TypePack::Explicit { type_list, .. } => type_list_may_contain_type_function(type_list),
        TypePack::Variadic { variadic_type, .. } => {
            annotation_may_contain_type_function(variadic_type)
        }
        TypePack::Generic { .. } => false,
    }
}

fn annotation_may_contain_type_function(annotation: &Type) -> bool {
    match annotation {
        Type::Reference { parameters, .. } => !parameters.is_empty(),
        Type::Typeof { .. } => true,
        Type::Group { inner, .. } => annotation_may_contain_type_function(inner),
        Type::Union { types, .. }
        | Type::Intersection { types, .. }
        | Type::Error { types, .. } => types.iter().any(annotation_may_contain_type_function),
        Type::Function {
            arg_types,
            return_types,
            ..
        } => {
            type_list_may_contain_type_function(arg_types)
                || type_pack_may_contain_type_function(return_types)
        }
        Type::Table { props, indexer, .. } => {
            props
                .iter()
                .any(|property| annotation_may_contain_type_function(&property.prop_type))
                || indexer.as_ref().is_some_and(|indexer| {
                    annotation_may_contain_type_function(&indexer.index_type)
                        || annotation_may_contain_type_function(&indexer.result_type)
                })
        }
        Type::Optional { .. } | Type::SingletonString { .. } | Type::SingletonBool { .. } => false,
    }
}

struct CompletedFunctionFrame {
    parameter_expectations: BTreeMap<LocalId, ParameterExpectations>,
    returned: bool,
    inferred_returns: Vec<InferredReturnPath>,
    inferred_return_seed: Option<TypePackId>,
    recursive_value_call_seen: bool,
    recursive_return_placeholder: Option<TypeId>,
}

#[allow(clippy::multiple_inherent_impl)]
impl<'a> ExpressionConstraintGenerator<'a> {
    fn with_function_frame<T>(
        &mut self,
        frame: FunctionFrame,
        infer_body: impl FnOnce(&mut Self) -> T,
    ) -> (T, CompletedFunctionFrame) {
        self.function_frames.return_stack.push(frame.return_pack);
        self.function_frames.vararg_stack.push(frame.vararg_pack);
        self.function_frames
            .parameter_expectation_stack
            .push(frame.parameter_expectations);
        self.function_frames
            .unannotated_return_stack
            .push(frame.unannotated_return);
        self.function_frames
            .contextual_return_stack
            .push(frame.contextual_return);
        self.function_frames.return_seen_stack.push(false);
        self.function_frames.inferred_return_stack.push(Vec::new());
        self.function_frames.inferred_return_seed_stack.push(None);
        self.function_frames
            .local_function_stack
            .push(frame.local_function_id);
        self.function_frames
            .global_function_stack
            .push(frame.global_function_name);
        self.function_frames
            .function_has_unannotated_parameter_stack
            .push(frame.has_unannotated_parameter);
        self.function_frames.recursive_value_call_stack.push(false);
        self.function_frames
            .recursive_return_placeholder_stack
            .push(frame.recursive_return_placeholder);
        self.function_frames
            .function_scope_stack
            .push(frame.function_scope);
        self.function_frames
            .function_is_global_stack
            .push(frame.function_is_global);

        let inferred = infer_body(self);

        self.function_frames
            .function_is_global_stack
            .pop()
            .expect("function frame missing function-is-global slot");
        self.function_frames
            .function_scope_stack
            .pop()
            .expect("function frame missing function scope");
        let recursive_return_placeholder = self
            .function_frames
            .recursive_return_placeholder_stack
            .pop()
            .expect("function frame missing recursive return placeholder");
        let recursive_value_call_seen = self
            .function_frames
            .recursive_value_call_stack
            .pop()
            .expect("function frame missing recursive-call marker");
        self.function_frames
            .function_has_unannotated_parameter_stack
            .pop()
            .expect("function frame missing unannotated-parameter marker");
        self.function_frames
            .global_function_stack
            .pop()
            .expect("function frame missing global function identity");
        self.function_frames
            .local_function_stack
            .pop()
            .expect("function frame missing local function identity");
        let inferred_returns = self
            .function_frames
            .inferred_return_stack
            .pop()
            .expect("function frame missing inferred returns");
        let inferred_return_seed = self
            .function_frames
            .inferred_return_seed_stack
            .pop()
            .expect("function frame missing inferred return seed");
        let returned = self
            .function_frames
            .return_seen_stack
            .pop()
            .expect("function frame missing return-seen marker");
        self.function_frames
            .contextual_return_stack
            .pop()
            .expect("function frame missing contextual-return marker");
        self.function_frames
            .unannotated_return_stack
            .pop()
            .expect("function frame missing unannotated-return marker");
        let parameter_expectations = self
            .function_frames
            .parameter_expectation_stack
            .pop()
            .expect("function frame missing parameter expectations");
        self.function_frames
            .vararg_stack
            .pop()
            .expect("function frame missing vararg pack");
        self.function_frames
            .return_stack
            .pop()
            .expect("function frame missing return pack");

        (
            inferred,
            CompletedFunctionFrame {
                parameter_expectations,
                returned,
                inferred_returns,
                inferred_return_seed,
                recursive_value_call_seen,
                recursive_return_placeholder,
            },
        )
    }

    pub(crate) fn expr_table(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: Option<Location>,
        items: &[TableItem],
    ) {
        let mut table = TableType::new(TableState::Unsealed);
        let prebound_table_ty = self.prebound_table_literals.remove(&expr.syntax_id());
        let table_ty =
            prebound_table_ty.unwrap_or_else(|| self.arena.alloc(TypeKind::Table(table.clone())));
        self.bind_actual(location, expr.syntax_id(), expr_ty, table_ty);
        let mut array_values = Vec::new();
        let expected_table = self
            .expected_by_syntax
            .get(&expr.syntax_id())
            .map(|expected| self.arena.follow(*expected))
            .and_then(|expected| self.expected_table_for_literal(expected, items));
        let suppress_primitive_inference = self.primitive_inference_suppressed_for_table(items);
        let mut suppress_array_indexer = false;
        let shadowed_items = shadowed_table_item_indices(items);
        for (item_index, item) in items.iter().enumerate() {
            let expected_item = if shadowed_items.contains(&item_index) {
                None
            } else {
                expected_table
                    .as_ref()
                    .and_then(|table| expected_table_item(table, item))
            };
            let expected_value = expected_item.map(|item| item.ty);
            let expression_expected_value =
                expected_item.and_then(|item| (!item.write_only).then_some(item.ty));
            let widened_value = widened_table_literal_value_type(self.arena, &item.value);
            let suppressed_widened_value = if suppress_primitive_inference {
                widened_value
            } else {
                None
            };
            let value_ty =
                if expression_expected_value.is_some() && suppressed_widened_value.is_some() {
                    self.expr_type_with_expected(scope, &item.value, None)
                } else {
                    self.expr_type_with_expected_aggregation(
                        scope,
                        &item.value,
                        expression_expected_value,
                        true,
                    )
                };
            if let (Some(expected), Some(widened)) =
                (expression_expected_value, suppressed_widened_value)
                && let Some(diagnostic) =
                    self.eager_table_primitive_literal_mismatch(widened, expected, &item.value)
            {
                self.generated.diagnostics.push(diagnostic);
            }
            let property_ty =
                if let (Some(expected), Some(_)) = (expected_value, suppressed_widened_value) {
                    expected
                } else {
                    expected_value.unwrap_or_else(|| widened_value.unwrap_or(value_ty))
                };
            match (&item.kind, &item.key) {
                (
                    TableItemKind::Record,
                    Some(Expr::String {
                        value, location, ..
                    }),
                ) => {
                    if let Some(indexer) =
                        self.expected_indexer_for_static_table_key(expected_table.as_ref(), value)
                    {
                        self.apply_contextual_literal_indexer(
                            &mut table,
                            indexer,
                            location.map(DiagnosticLocation::from),
                        );
                    } else {
                        table.properties.insert(
                            value.clone(),
                            TableProperty::new(property_ty)
                                .with_location(location.map(DiagnosticLocation::from)),
                        );
                    }
                }
                (TableItemKind::Record, Some(Expr::Global { name, location, .. })) => {
                    if let Some(indexer) = self.expected_indexer_for_static_table_key(
                        expected_table.as_ref(),
                        name.as_str(),
                    ) {
                        self.apply_contextual_literal_indexer(
                            &mut table,
                            indexer,
                            location.map(DiagnosticLocation::from),
                        );
                    } else {
                        table.properties.insert(
                            name.as_str().to_owned(),
                            TableProperty::new(property_ty)
                                .with_location(location.map(DiagnosticLocation::from)),
                        );
                    }
                }
                (TableItemKind::General, Some(key)) => {
                    let key_ty = self.expr_type(scope, key);
                    let expected_indexer = expected_table
                        .as_ref()
                        .and_then(|table| table.indexer.clone());
                    let expected_static_property = match key {
                        Expr::String { value, .. } => expected_table
                            .as_ref()
                            .is_some_and(|table| table.properties.contains_key(value)),
                        _ => false,
                    };
                    if let Some(indexer) = expected_indexer.as_ref()
                        && !expected_static_property
                    {
                        self.constrain_contextual_literal_indexer_key(key_ty, indexer.key, key);
                    }
                    if let Expr::String {
                        value, location, ..
                    } = key
                    {
                        if let Some(indexer) = self
                            .expected_indexer_for_static_table_key(expected_table.as_ref(), value)
                        {
                            self.apply_contextual_literal_indexer(
                                &mut table,
                                indexer,
                                location.map(DiagnosticLocation::from),
                            );
                        } else {
                            table.properties.insert(
                                value.clone(),
                                TableProperty::new(property_ty)
                                    .with_location(location.map(DiagnosticLocation::from)),
                            );
                        }
                    } else if let Some(indexer) = expected_indexer {
                        self.apply_contextual_literal_indexer(
                            &mut table,
                            indexer,
                            key.location().map(DiagnosticLocation::from),
                        );
                    } else if let Some(indexer) = table.indexer.clone() {
                        self.generated
                            .constraints
                            .push(Constraint::subtype_default_location(
                                key_ty,
                                indexer.key,
                                key.location().map(DiagnosticLocation::from),
                            ));
                        self.generated
                            .constraints
                            .push(Constraint::subtype_default_location(
                                value_ty,
                                indexer.value,
                                item.value.location().map(DiagnosticLocation::from),
                            ));
                    } else {
                        table.indexer = Some(TableIndexer {
                            key: key_ty,
                            value: property_ty,
                            read_only: false,
                        });
                    }
                }
                _ => {
                    if let Some(indexer) = expected_table
                        .as_ref()
                        .and_then(|table| table.indexer.as_ref())
                        && Subtyper::new(self.arena)
                            .is_subtype(self.primitives().number, indexer.key)
                            .is_err()
                    {
                        self.generated.diagnostics.push(
                            Diagnostic::error(
                                DiagnosticCategory::TypeMismatch,
                                DiagnosticLocation::from_opt(item.value.location()),
                            )
                            .with_context(
                                "Unexpected array-like table item: the indexer key type \
                                 of this table is not `number`.",
                            ),
                        );
                        suppress_array_indexer = true;
                    }
                    array_values.push((
                        property_ty,
                        item.value.location().map(DiagnosticLocation::from),
                    ));
                }
            }
        }
        self.finalize_table_array_indexer(
            &mut table,
            &array_values,
            suppress_array_indexer,
            expected_table.as_ref(),
        );
        self.extract_prebound_operator_metamethods(
            &mut table,
            table_ty,
            prebound_table_ty.is_some(),
        );
        self.arena.replace(table_ty, TypeKind::Table(table));
        self.record_actual(location, expr.syntax_id(), table_ty);
    }

    /// Resolves the array-portion indexer of a table literal: honour an
    /// expected indexer when one is supplied, otherwise synthesise a `number`
    /// indexer from the collected element types. Also inherits a leftover
    /// expected indexer when the literal produced none of its own.
    fn finalize_table_array_indexer(
        &mut self,
        table: &mut TableType,
        array_values: &[(TypeId, Option<DiagnosticLocation>)],
        suppress_array_indexer: bool,
        expected_table: Option<&TableType>,
    ) {
        if !suppress_array_indexer && !array_values.is_empty() {
            if let Some(expected_indexer) = expected_table.and_then(|table| table.indexer.clone()) {
                for (value, location) in array_values {
                    self.generated
                        .constraints
                        .push(Constraint::subtype_default_location(
                            *value,
                            expected_indexer.value,
                            *location,
                        ));
                }
                self.apply_contextual_literal_indexer(
                    table,
                    expected_indexer,
                    array_values.first().and_then(|(_, location)| *location),
                );
            } else if expected_table.is_some() {
                let (first, first_location) = array_values[0];
                table.indexer = Some(TableIndexer {
                    key: self.primitives().number,
                    value: first,
                    read_only: false,
                });
                for (value, location) in &array_values[1..] {
                    if *value != first {
                        self.generated
                            .constraints
                            .push(Constraint::subtype_default_location(
                                *value,
                                first,
                                (*location).or(first_location),
                            ));
                    }
                }
            } else {
                let value = self.union_type(array_values.iter().map(|(value, _)| *value).collect());
                let value = simplify_type(self.arena, value);
                table.indexer = Some(TableIndexer {
                    key: self.primitives().number,
                    value,
                    read_only: false,
                });
            }
        }
        if table.indexer.is_none()
            && let Some(indexer) = expected_table.and_then(|table| table.indexer.clone())
        {
            table.indexer = Some(indexer);
        }
    }

    /// Hoists operator metamethod names written into a prebound table literal
    /// out to the deferred unsealed-property-write map so they reach the bound
    /// table type after solving.
    fn extract_prebound_operator_metamethods(
        &mut self,
        table: &mut TableType,
        table_ty: TypeId,
        prebound: bool,
    ) {
        if !prebound {
            return;
        }
        let operator_properties = table
            .properties
            .keys()
            .filter(|name| is_operator_metamethod_name(name))
            .cloned()
            .collect::<Vec<_>>();
        for name in operator_properties {
            if let Some(property) = table.properties.remove(&name) {
                self.table_writes
                    .unsealed_property_writes
                    .entry(table_ty)
                    .or_default()
                    .entry(name)
                    .or_insert(property.ty);
            }
        }
    }
    fn primitive_inference_suppressed_for_table(&self, items: &[TableItem]) -> bool {
        let limit = self.input.config.primitive_inference_table_limit;
        items
            .iter()
            .filter(|item| widened_table_literal_value_type(self.arena, &item.value).is_some())
            .nth(limit)
            .is_some()
    }
    fn expected_indexer_for_static_table_key(
        &mut self,
        expected_table: Option<&TableType>,
        key: &str,
    ) -> Option<TableIndexer> {
        let table = expected_table?;
        if table.properties.contains_key(key) {
            return None;
        }
        let indexer = table.indexer.clone()?;
        let key_ty = self
            .arena
            .alloc(TypeKind::Singleton(SingletonType::String(key.to_owned())));
        Subtyper::new(self.arena)
            .is_subtype(key_ty, indexer.key)
            .is_ok()
            .then_some(indexer)
    }
    fn constrain_contextual_literal_indexer_key(
        &mut self,
        key_ty: TypeId,
        expected_key: TypeId,
        key: &Expr,
    ) {
        if self.is_dynamic(key_ty) {
            return;
        }
        if let Some(diagnostic) = self.eager_table_indexer_key_mismatch(key_ty, expected_key, key) {
            self.generated.diagnostics.push(diagnostic);
        } else {
            self.generated
                .constraints
                .push(Constraint::subtype_default_location(
                    key_ty,
                    expected_key,
                    key.location().map(DiagnosticLocation::from),
                ));
        }
    }
    fn apply_contextual_literal_indexer(
        &mut self,
        table: &mut TableType,
        expected_indexer: TableIndexer,
        location: Option<DiagnosticLocation>,
    ) {
        if let Some(existing) = table.indexer.clone() {
            self.generated
                .constraints
                .push(Constraint::subtype_default_location(
                    existing.key,
                    expected_indexer.key,
                    location,
                ));
            self.generated
                .constraints
                .push(Constraint::subtype_default_location(
                    existing.value,
                    expected_indexer.value,
                    location,
                ));
        }
        table.indexer = Some(expected_indexer);
    }
    fn eager_table_primitive_literal_mismatch(
        &self,
        widened_ty: TypeId,
        expected_ty: TypeId,
        value: &Expr,
    ) -> Option<Diagnostic> {
        let error = Subtyper::new(self.arena)
            .is_subtype(widened_ty, expected_ty)
            .err()?;
        let mut diagnostic = ConstraintSolveError::Subtype(error).into_diagnostic();
        diagnostic.primary_location = DiagnosticLocation::from_opt(value.location());
        Some(diagnostic.with_context(
            "Primitive literal was widened because this table literal exceeds \
             the primitive inference limit.",
        ))
    }
    fn contextual_function_parameter_type(
        &mut self,
        scope: ScopeId,
        local: &Local,
        contextual_parameters: Option<&ExpectedCallParameterPack>,
        parameter_index: usize,
        force_query_unknown: bool,
    ) -> TypeId {
        let parameter = self.local_annotation_or_free(scope, local);
        if local.annotation.is_some() {
            return parameter;
        }
        let Some(expected) = contextual_parameters
            .and_then(|parameters| parameters.parameter_at(self.arena, parameter_index))
        else {
            return parameter;
        };
        if force_query_unknown {
            self.query_capture
                .generic_contextual_callback_locals
                .insert(local.id);
        }
        if self.local_type_can_bind_expected(parameter)
            && !self.type_contains_type(parameter, expected, &mut BTreeSet::new())
        {
            self.bind_free_to(parameter, expected);
        }
        parameter
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn expr_function(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
        expr_ty: TypeId,
        location: Option<Location>,
        generics: &[ruau_syntax::GenericType],
        generic_packs: &[ruau_syntax::GenericTypePack],
        args: &[Local],
        self_arg: Option<&Local>,
        vararg: bool,
        vararg_annotation: Option<&TypePack>,
        return_annotation: Option<&TypePack>,
        body: &Stat,
    ) {
        let contextual_function = self
            .expected_by_syntax
            .get(&expr.syntax_id())
            .copied()
            .and_then(|expected| {
                self.expected_function_for_function_literal(
                    expected,
                    args.len() + usize::from(self_arg.is_some()),
                    vararg,
                )
            });
        let contextual_returns = contextual_function.and_then(|expected| {
            ExpectedFunctionReturnPack::from_expected_type(self.arena, expected)
        });
        let contextual_parameters = contextual_function.map(|expected| {
            ExpectedCallParameterPack::from_callee(
                self.arena,
                expected,
                ReceiverParameter::Explicit,
            )
        });
        let generic_query_parameters = self
            .query_capture
            .generic_contextual_callback_parameters
            .remove(&expr.syntax_id())
            .unwrap_or_default();
        self.report_duplicate_generic_parameters(generics, generic_packs, location);
        let can_ascribe_contextual_function = contextual_function.is_some()
            && !contextual_function.is_some_and(|expected| {
                self.type_contains_free_or_generic(
                    expected,
                    &mut BTreeSet::new(),
                    &mut BTreeSet::new(),
                )
            })
            && !self
                .non_ascribing_contextual_functions
                .contains(&expr.syntax_id())
            && self_arg.is_none()
            && generics.is_empty()
            && generic_packs.is_empty()
            && vararg_annotation.is_none()
            && return_annotation.is_none()
            && args.iter().all(|arg| arg.annotation.is_none());
        let has_contextual_function_type = contextual_returns.is_some();
        let function_scope = self.enter_child(scope);
        let (function_generics, _) = self.function_type_generic_substitutions(generics);
        let (function_generic_packs, _) =
            self.function_type_generic_pack_substitutions(generic_packs);
        self.expr_function_with_typed_state(
            expr,
            expr_ty,
            location,
            args,
            self_arg,
            vararg,
            vararg_annotation,
            return_annotation,
            body,
            contextual_function,
            contextual_returns,
            &contextual_parameters,
            &generic_query_parameters,
            can_ascribe_contextual_function,
            has_contextual_function_type,
            function_scope,
            function_generics,
            function_generic_packs,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn expr_function_with_typed_state(
        &mut self,
        expr: &Expr,
        expr_ty: TypeId,
        location: Option<Location>,
        args: &[Local],
        self_arg: Option<&Local>,
        vararg: bool,
        vararg_annotation: Option<&TypePack>,
        return_annotation: Option<&TypePack>,
        body: &Stat,
        contextual_function: Option<TypeId>,
        contextual_returns: Option<ExpectedFunctionReturnPack>,
        contextual_parameters: &Option<ExpectedCallParameterPack>,
        generic_query_parameters: &BTreeSet<usize>,
        can_ascribe_contextual_function: bool,
        has_contextual_function_type: bool,
        function_scope: ScopeId,
        function_generics: Vec<GenericType>,
        function_generic_packs: Vec<GenericTypePack>,
    ) {
        let scan_uninhabited_annotations = self_arg
            .and_then(|arg| arg.annotation.as_ref())
            .is_some_and(|annotation| annotation_may_contain_type_function(annotation))
            || args
                .iter()
                .filter_map(|arg| arg.annotation.as_ref())
                .any(|annotation| annotation_may_contain_type_function(annotation))
            || vararg_annotation.is_some_and(type_pack_may_contain_type_function)
            || return_annotation.is_some_and(type_pack_may_contain_type_function);
        let mut argument_names = Vec::new();
        let mut arguments = Vec::new();
        let mut parameter_expectations = BTreeMap::new();
        let mut parameter_index = 0;
        let mut self_argument_type = None;
        let (function_is_global, local_function_id, global_function_name) =
            self.take_pending_function_identity();
        if let Some(self_arg) = self_arg {
            argument_names.push(Some(self_arg.name.as_str().to_owned()));
            let argument = self.contextual_function_parameter_type(
                function_scope,
                self_arg,
                contextual_parameters.as_ref(),
                parameter_index,
                generic_query_parameters.contains(&parameter_index),
            );
            self_argument_type = Some(argument);
            arguments.push(argument);
            parameter_expectations.insert(
                self_arg.id,
                ParameterExpectations {
                    declaration_location: self_arg.location.map(DiagnosticLocation::from),
                    expectations: Vec::new(),
                },
            );
            parameter_index += 1;
        }
        for arg in args {
            argument_names.push(Some(arg.name.as_str().to_owned()));
            let argument = self.contextual_function_parameter_type(
                function_scope,
                arg,
                contextual_parameters.as_ref(),
                parameter_index,
                generic_query_parameters.contains(&parameter_index),
            );
            arguments.push(argument);
            parameter_expectations.insert(
                arg.id,
                ParameterExpectations {
                    declaration_location: arg.location.map(DiagnosticLocation::from),
                    expectations: Vec::new(),
                },
            );
            parameter_index += 1;
        }
        let contextual_return_pack = if return_annotation.is_none() {
            contextual_returns.map(ExpectedFunctionReturnPack::returns)
        } else {
            None
        };
        let tail = if vararg {
            Some(self.with_generic_alias_type_arguments(|this| {
                this.lower_vararg_type_pack_option(function_scope, vararg_annotation)
            }))
        } else {
            None
        };
        let mut return_pack = if let Some(return_annotation) = return_annotation {
            self.with_function_signature_lowering(|this| {
                this.lower_type_pack(function_scope, return_annotation)
            })
        } else if let Some(contextual_return_pack) = contextual_return_pack {
            contextual_return_pack
        } else {
            self.arena.alloc_pack(TypePackKind::Free {
                level: TypeLevel(0),
                name: None,
            })
        };
        let argument_pack = self.arena.alloc_pack(TypePackKind::List {
            types: arguments,
            tail,
        });
        let recursive_return_placeholder = (return_annotation.is_none()
            && (local_function_id.is_some() || global_function_name.is_some()))
        .then(|| self.arena.alloc(TypeKind::Error));
        let has_unannotated_parameter = self_arg.is_some_and(|arg| arg.annotation.is_none())
            || args.iter().any(|arg| arg.annotation.is_none())
            || (vararg && vararg_annotation.is_none());
        let frame = FunctionFrame {
            return_pack,
            vararg_pack: tail,
            parameter_expectations,
            unannotated_return: return_annotation.is_none(),
            contextual_return: contextual_return_pack.is_some(),
            local_function_id,
            global_function_name,
            has_unannotated_parameter,
            recursive_return_placeholder,
            function_scope,
            function_is_global,
        };
        let (_, completed_frame) = self.with_function_frame(frame, |this| {
            let body_scope = this.enter_child(function_scope);
            this.visit_stat(body_scope, body);
        });
        let CompletedFunctionFrame {
            parameter_expectations,
            returned,
            inferred_returns,
            inferred_return_seed,
            recursive_value_call_seen,
            recursive_return_placeholder,
        } = completed_frame;
        self.resolve_function_parameter_expectations(&parameter_expectations);
        let no_return_demanded_return_pack = return_annotation.is_none()
            && !returned
            && self.pack_has_demanded_return_value(return_pack);
        let no_return_recursive_value_call = return_annotation.is_none()
            && !returned
            && recursive_value_call_seen
            && !self.stat_always_exits(body);
        let no_return_preserves_return_pack =
            no_return_demanded_return_pack || no_return_recursive_value_call;
        if return_annotation.is_none() {
            return_pack = if returned {
                if let Some(contextual_returns) =
                    contextual_returns.filter(|returns| returns.is_tail_only(self.arena))
                {
                    contextual_returns.returns()
                } else if let Some(seed) = inferred_return_seed {
                    seed
                } else {
                    self.inferred_return_pack(&inferred_returns, !has_contextual_function_type)
                }
            } else if let Some(contextual_returns) = contextual_returns {
                contextual_returns.returns()
            } else if no_return_preserves_return_pack {
                return_pack
            } else {
                self.arena.empty_pack()
            };
        }
        if recursive_value_call_seen {
            return_pack =
                self.close_recursive_return_placeholder(recursive_return_placeholder, return_pack);
        }
        if let Some(self_argument_type) = self_argument_type {
            self.settle_function_parameter_surface(self_argument_type, return_pack);
        }
        let contextual_pack_allows_empty_body = return_annotation.is_none()
            && !returned
            && contextual_returns.is_some_and(|returns| returns.allows_empty_body(self.arena));
        if (self.input.mode == Mode::Strict || no_return_preserves_return_pack)
            && self.pack_requires_return_value(return_pack)
            && !contextual_pack_allows_empty_body
            && !self.stat_always_exits(body)
        {
            self.generated.diagnostics.push(
                Diagnostic::error(
                    DiagnosticCategory::Generic,
                    DiagnosticLocation::from_opt(location),
                )
                .with_typed(crate::diagnostics::Payload::FunctionExitsWithoutReturning),
            );
        }
        let mut function = FunctionType::new(argument_pack, return_pack);
        function.argument_names = argument_names;
        function.has_self = self_arg.is_some();
        function.is_checked = true;
        function.generics = function_generics;
        function.generic_packs = function_generic_packs;
        if scan_uninhabited_annotations {
            self.report_uninhabited_type_function_diagnostics_for_function(&function, location);
        }
        let function_ty = match contextual_function {
            Some(expected) if can_ascribe_contextual_function => expected,
            _ => self.arena.alloc(TypeKind::Function(function)),
        };
        self.bind_actual(location, expr.syntax_id(), expr_ty, function_ty);
    }

    fn report_duplicate_generic_parameters(
        &mut self,
        generics: &[ruau_syntax::GenericType],
        generic_packs: &[ruau_syntax::GenericTypePack],
        location: Option<Location>,
    ) {
        let mut seen = BTreeSet::new();
        for name in generics
            .iter()
            .map(|generic| generic.name.as_str())
            .chain(generic_packs.iter().map(|generic| generic.name.as_str()))
        {
            if !seen.insert(name.to_owned()) {
                self.generated.diagnostics.push(
                    Diagnostic::error(
                        DiagnosticCategory::Generic,
                        DiagnosticLocation::from_opt(location),
                    )
                    .with_typed(
                        crate::diagnostics::Payload::DuplicateGenericParameterName {
                            name: name.to_owned(),
                        },
                    ),
                );
                return;
            }
        }
    }

    fn pack_has_demanded_return_value(&self, pack: TypePackId) -> bool {
        if matches!(
            self.arena.get_pack(self.arena.follow_pack(pack)),
            TypePackKind::Free { .. }
        ) {
            return false;
        }
        self.pack_requires_return_value(pack)
    }

    fn close_recursive_return_placeholder(
        &mut self,
        placeholder: Option<TypeId>,
        return_pack: TypePackId,
    ) -> TypePackId {
        let Some(placeholder) = placeholder else {
            return return_pack;
        };
        let normalized = self.arena.normalize_pack(return_pack);
        let [seed] = normalized.types.as_slice() else {
            return return_pack;
        };
        if normalized.tail.is_some()
            || *seed == placeholder
            || !self.type_contains_type(placeholder, *seed, &mut BTreeSet::new())
        {
            return return_pack;
        }
        let replacement = self.arena.get(self.arena.follow(*seed)).clone();
        self.arena.replace(placeholder, replacement);
        self.pack(vec![placeholder])
    }

    fn eager_table_indexer_key_mismatch(
        &self,
        key_ty: TypeId,
        indexer_key_ty: TypeId,
        key: &Expr,
    ) -> Option<Diagnostic> {
        let error = Subtyper::new(self.arena)
            .is_subtype(key_ty, indexer_key_ty)
            .err()?;
        let mut diagnostic =
            ConstraintSolveError::Subtype(error).into_diagnostic_with_arena(Some(self.arena));
        diagnostic.primary_location = DiagnosticLocation::from_opt(key.location());
        Some(diagnostic)
    }
}
