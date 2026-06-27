//! `for ... in` iterator type inference for expression constraint generation.
//!
//! Determines the value types a generic `for` loop binds from its iterator
//! expression: the iterator-function protocol, metatable `__iter`/`__call`
//! iterators, `pairs`/`ipairs`/`next` builtins, and direct table iteration,
//! including the strict/nonstrict recovery rules for non-iterable values.

use ruau_ast::syntax::Expr;

use crate::{
    constraints::Constraint,
    diagnostic::{DiagnosticCategory, DiagnosticLocation, Payload, TypeDiagnostic},
    generation::state::ExpressionConstraintGenerator,
    scopes::ScopeId,
    types::{
        PrimitiveType, SingletonType, TableIndexer, TableState, TableType, TypeId, TypeKind,
        TypePackTail,
    },
};

#[allow(clippy::multiple_inherent_impl)]
impl<'a> ExpressionConstraintGenerator<'a> {
    pub(crate) fn report_zero_value_for_in_iterator(
        &mut self,
        scope: ScopeId,
        values: &[Expr],
        value_types: &[TypeId],
    ) -> bool {
        let location = if let [Expr::Call { location, func, .. }] = values {
            let callee = self.dfg_type_for_expr(func);
            let expected_callee = self.callable_type(scope, func, callee);
            (self.function_fixed_return_count(expected_callee) == Some(0))
                .then(|| location.map(DiagnosticLocation::from))
        } else if let [value] = values {
            value_types
                .first()
                .is_some_and(|ty| self.function_fixed_return_count(*ty) == Some(0))
                .then(|| value.location().map(DiagnosticLocation::from))
        } else {
            None
        };
        if values.len() != 1 {
            return false;
        }
        let Some(location) = location else {
            return false;
        };
        let location = location.unwrap_or_else(DiagnosticLocation::missing);
        self.generated.diagnostics.push(
            TypeDiagnostic::error(DiagnosticCategory::Constraint, location).with_context(
                "for..in loops require at least one value to iterate over.  Got zero",
            ),
        );
        true
    }
    pub(crate) fn constrain_for_in_iterator_arguments(
        &mut self,
        value_exprs: &[Expr],
        values: &[TypeId],
    ) {
        let Some(iterator) = values.first().copied() else {
            return;
        };
        if self.is_dynamic(iterator) {
            return;
        }
        let argument_types = self.function_argument_types(iterator);
        for (actual_expr, actual, expected) in value_exprs
            .iter()
            .skip(1)
            .zip(values.iter().skip(1).copied())
            .zip(argument_types)
            .map(|((actual_expr, actual), expected)| (actual_expr, actual, expected))
        {
            if self.is_dynamic(actual) || self.is_dynamic(expected) {
                continue;
            }
            self.generated
                .constraints
                .push(Constraint::subtype_default_location(
                    actual,
                    expected,
                    actual_expr.location().map(DiagnosticLocation::from),
                ));
            break;
        }
    }
    pub(crate) fn for_in_loop_value_types(
        &mut self,
        value_exprs: &[Expr],
        values: &[TypeId],
        var_count: usize,
        zero_value_iterator_reported: bool,
    ) -> Option<Vec<TypeId>> {
        if values.is_empty() {
            return None;
        }
        let mut adjusted_values;
        let values = if self.arena.is_optional(values[0]) && !self.is_dynamic(values[0]) {
            let location = value_exprs.first().and_then(Expr::location);
            self.report_nilable_type_mismatch(values[0], location);
            adjusted_values = values.to_vec();
            adjusted_values[0] = self.strip_nil(values[0]);
            adjusted_values.as_slice()
        } else {
            values
        };
        if matches!(
            self.arena.get(self.arena.follow(values[0])),
            TypeKind::Never
        ) {
            return Some(vec![self.primitives().never; var_count]);
        }
        if matches!(
            self.arena.get(self.arena.follow(values[0])),
            TypeKind::Primitive(PrimitiveType::Nil)
        ) {
            if !zero_value_iterator_reported {
                let location = value_exprs
                    .first()
                    .and_then(Expr::location)
                    .map(DiagnosticLocation::from)
                    .unwrap_or_else(DiagnosticLocation::missing);
                self.generated.diagnostics.push(
                    TypeDiagnostic::error(DiagnosticCategory::Call, location)
                        .with_context("for..in loop iterator resolved to nil"),
                );
            }
            return Some(vec![self.primitives().error; var_count]);
        }
        if let Some(loop_values) =
            self.for_in_builtin_table_iteration_types(value_exprs, values, var_count)
        {
            return Some(loop_values);
        }
        if let Some(loop_values) =
            self.for_in_metatable_iterator_values(value_exprs, values[0], var_count)
        {
            return Some(loop_values);
        }
        if let Some((key, value)) = self.for_in_table_iteration_types(values[0]) {
            let nil = self.primitives().nil;
            return Some(
                (0..var_count)
                    .map(|index| match index {
                        0 => key,
                        1 => value,
                        _ => nil,
                    })
                    .collect(),
            );
        }
        let iterator_location = value_exprs
            .first()
            .and_then(Expr::location)
            .map(DiagnosticLocation::from);
        if let Some(loop_values) =
            self.for_in_iterator_function_values(values[0], var_count, iterator_location)
        {
            return Some(loop_values);
        }
        if let Some(loop_values) = self.for_in_pairs_dynamic_argument_values(value_exprs, var_count)
        {
            return Some(loop_values);
        }
        if values.iter().any(|ty| self.is_error_type(*ty)) {
            let error = self.primitives().error;
            let recovery = if matches!(value_exprs.first(), Some(Expr::Call { .. })) {
                error
            } else {
                self.union_type(vec![error, self.primitives().nil])
            };
            return Some(vec![recovery; var_count]);
        }
        if values
            .iter()
            .any(|ty| self.is_dynamic(*ty) || self.is_function_type(*ty))
        {
            let recovery = self.for_in_dynamic_key_type();
            return Some(vec![recovery; var_count]);
        }
        None
    }
    pub(crate) fn is_for_in_dynamic_recovery(&self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        let TypeKind::Union(types) = self.arena.get(ty) else {
            return false;
        };
        let primitives = self.primitives();
        let mut has_error = false;
        let mut has_non_nil = false;
        for ty in types {
            let ty = self.arena.follow(*ty);
            match self.arena.get(ty) {
                TypeKind::Error if ty == primitives.error => has_error = true,
                TypeKind::Primitive(PrimitiveType::Nil) if ty == primitives.nil => {}
                TypeKind::Negation(inner) if self.arena.follow(*inner) == primitives.nil => {
                    has_non_nil = true;
                }
                _ => return false,
            }
        }
        has_error && has_non_nil
    }
    pub(crate) fn for_in_iterator_function_values(
        &mut self,
        iterator: TypeId,
        var_count: usize,
        location: Option<DiagnosticLocation>,
    ) -> Option<Vec<TypeId>> {
        let iterator = self.arena.follow(iterator);
        let TypeKind::Function(function) = self.arena.get(iterator) else {
            return None;
        };
        let returns = self.arena.normalize_pack(function.returns);
        if returns.types.is_empty() && returns.tail.is_none() {
            let location = location.unwrap_or_else(DiagnosticLocation::missing);
            self.generated.diagnostics.push(
                TypeDiagnostic::error(DiagnosticCategory::Constraint, location).with_context(
                    "for..in loops require at least one value to iterate over.  Got zero",
                ),
            );
            return Some(vec![self.primitives().error; var_count]);
        }
        if returns.types.iter().any(|ty| self.is_dynamic(*ty))
            || matches!(
                returns.tail,
                Some(TypePackTail::Variadic(ty)) if self.is_dynamic(ty)
            )
            || matches!(returns.tail, Some(TypePackTail::Error))
        {
            return None;
        }
        let nil = self.primitives().nil;
        Some(
            (0..var_count)
                .map(|index| {
                    returns
                        .types
                        .get(index)
                        .copied()
                        .or_else(|| match returns.tail {
                            Some(TypePackTail::Variadic(ty)) => Some(ty),
                            Some(TypePackTail::Error) => Some(self.primitives().error),
                            _ => None,
                        })
                        .unwrap_or(nil)
                })
                .collect(),
        )
    }
    pub(crate) fn for_in_metatable_iterator_values(
        &mut self,
        value_exprs: &[Expr],
        iterable: TypeId,
        var_count: usize,
    ) -> Option<Vec<TypeId>> {
        let iterable = self.arena.follow(iterable);
        let TypeKind::Metatable { metatable, .. } = self.arena.get(iterable).clone() else {
            return None;
        };
        let metatable = self.arena.follow(metatable);
        let TypeKind::Table(metatable) = self.arena.get(metatable).clone() else {
            return None;
        };
        let iter_ty = metatable.properties.get("__iter")?.ty;
        let iter_ty = self.arena.follow(iter_ty);
        let TypeKind::Function(iter_function) = self.arena.get(iter_ty).clone() else {
            return None;
        };
        let returns = self.arena.normalize_pack(iter_function.returns);
        let next_ty = returns
            .types
            .first()
            .copied()
            .or_else(|| match returns.tail {
                Some(TypePackTail::Variadic(ty)) => Some(ty),
                Some(TypePackTail::Error) => Some(self.primitives().error),
                _ => None,
            })?;
        if matches!(
            self.arena.get(self.arena.follow(next_ty)),
            TypeKind::Unknown
        ) {
            let location = value_exprs
                .first()
                .and_then(Expr::location)
                .map(DiagnosticLocation::from)
                .unwrap_or_else(DiagnosticLocation::missing);
            let diagnostic = TypeDiagnostic::error(DiagnosticCategory::Call, location)
                .with_context("__iter metamethod returned unknown iterator function")
                .with_typed(Payload::NotCallable);
            self.generated.diagnostics.push(diagnostic);
            return Some(vec![self.primitives().error; var_count]);
        }
        let supplied_iterator_args = returns.types.len().saturating_sub(1);
        let required_iterator_args = self.function_required_argument_count(next_ty);
        if required_iterator_args > supplied_iterator_args {
            let location = value_exprs
                .first()
                .and_then(Expr::location)
                .map(DiagnosticLocation::from)
                .unwrap_or_else(DiagnosticLocation::missing);
            self.generated.diagnostics.push(
                TypeDiagnostic::error(DiagnosticCategory::Generic, location)
                    .with_context("__iter metamethod must return (next[, table[, state]])")
                    .with_typed(Payload::IterMetamethodMissingState {
                        required: required_iterator_args,
                        provided: supplied_iterator_args,
                    }),
            );
            return Some(vec![self.primitives().error; var_count]);
        }
        self.for_in_iterator_function_values(next_ty, var_count, None)
    }
    pub(crate) fn for_in_builtin_table_iteration_types(
        &mut self,
        value_exprs: &[Expr],
        values: &[TypeId],
        var_count: usize,
    ) -> Option<Vec<TypeId>> {
        match value_exprs {
            [Expr::Call { func, args, .. }, ..] if matches!(func.as_ref(), Expr::Global { name, .. } if name.as_str() == "pairs") =>
            {
                let arg_ty = self.dfg_type_for_expr(args.first()?);
                let (key, value) = self.builtin_table_iteration_types(arg_ty, true)?;
                Some(self.for_in_loop_values(key, value, var_count))
            }
            [Expr::Call { func, args, .. }, ..] if matches!(func.as_ref(), Expr::Global { name, .. } if name.as_str() == "ipairs") =>
            {
                let arg_ty = self.dfg_type_for_expr(args.first()?);
                let value = self
                    .for_in_table_iteration_types(arg_ty)
                    .map(|(_, value)| value)
                    .unwrap_or_else(|| self.primitives().any);
                Some(self.for_in_loop_values(self.primitives().number, value, var_count))
            }
            [Expr::Global { name, .. }, _, ..] if name.as_str() == "next" => {
                let table_ty = *values.get(1)?;
                let (key, value) = self.builtin_table_iteration_types(table_ty, false)?;
                Some(self.for_in_loop_values(key, value, var_count))
            }
            _ => None,
        }
    }
    pub(crate) fn for_in_loop_values(
        &self,
        key: TypeId,
        value: TypeId,
        var_count: usize,
    ) -> Vec<TypeId> {
        let nil = self.primitives().nil;
        (0..var_count)
            .map(|index| match index {
                0 => key,
                1 => value,
                _ => nil,
            })
            .collect()
    }
    pub(crate) fn for_in_table_iteration_types(&mut self, ty: TypeId) -> Option<(TypeId, TypeId)> {
        self.for_in_table_iteration_types_with(ty, false)
    }
    pub(crate) fn for_in_table_iteration_types_with(
        &mut self,
        ty: TypeId,
        install_pairs_indexer: bool,
    ) -> Option<(TypeId, TypeId)> {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Table(mut table) => {
                if let Some(indexer) = table.indexer.clone() {
                    return Some((indexer.key, self.strip_nil(indexer.value)));
                }
                let mut property_keys = Vec::new();
                let mut property_values = Vec::new();
                for (name, property) in &table.properties {
                    property_keys.push(
                        self.arena
                            .alloc(TypeKind::Singleton(SingletonType::String(name.clone()))),
                    );
                    property_values.push(property.ty);
                }
                if property_keys.is_empty() {
                    if table.name.as_deref() == Some("table") {
                        let key = self.arena.alloc(TypeKind::Negation(self.primitives().nil));
                        Some((key, self.primitives().unknown))
                    } else if install_pairs_indexer && table.is_unsealed() {
                        Some((self.primitives().string, self.primitives().any))
                    } else {
                        None
                    }
                } else {
                    let key = self.union_type(property_keys);
                    let value = self.union_type(property_values);
                    let value = self.strip_nil(value);
                    if install_pairs_indexer && table.is_unsealed() {
                        table.indexer = Some(crate::types::TableIndexer {
                            key: self.primitives().string,
                            value: self.primitives().any,
                            read_only: false,
                        });
                        self.arena.replace(ty, TypeKind::Table(table));
                    }
                    Some((key, value))
                }
            }
            TypeKind::Union(types) => {
                let mut keys = Vec::new();
                let mut values = Vec::new();
                for ty in types {
                    let (key, value) =
                        self.for_in_table_iteration_types_with(ty, install_pairs_indexer)?;
                    keys.push(key);
                    values.push(value);
                }
                Some((self.union_type(keys), self.union_type(values)))
            }
            TypeKind::Metatable { table, .. } => {
                self.for_in_table_iteration_types_with(table, install_pairs_indexer)
            }
            _ => None,
        }
    }
    pub(crate) fn builtin_table_iteration_types(
        &mut self,
        ty: TypeId,
        install_pairs_indexer: bool,
    ) -> Option<(TypeId, TypeId)> {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Error | TypeKind::Never => None,
            TypeKind::Union(types) => {
                let mut keys = Vec::new();
                let mut values = Vec::new();
                for ty in types {
                    let ty = self.arena.follow(ty);
                    if matches!(self.arena.get(ty), TypeKind::Error | TypeKind::Never) {
                        continue;
                    }
                    let (key, value) =
                        self.builtin_table_iteration_types(ty, install_pairs_indexer)?;
                    keys.push(key);
                    values.push(value);
                }
                (!keys.is_empty()).then(|| (self.union_type(keys), self.union_type(values)))
            }
            TypeKind::Table(table) if self.is_error_suppressed_dynamic_table(&table) => {
                Some((self.primitives().unknown, self.primitives().unknown))
            }
            _ => self.for_in_table_iteration_types_with(ty, install_pairs_indexer),
        }
    }
    pub(crate) fn pairs_state_type(&mut self, ty: TypeId) -> TypeId {
        if self.type_contains_error_suppressed_dynamic_table(ty) {
            self.unknown_iteration_table_type()
        } else {
            ty
        }
    }
    pub(crate) fn unknown_iteration_table_type(&mut self) -> TypeId {
        let primitives = self.primitives();
        let mut table = TableType::new(TableState::Sealed);
        table.name = Some("{+ [unknown]: unknown +}".to_owned());
        table.indexer = Some(TableIndexer {
            key: primitives.unknown,
            value: primitives.unknown,
            read_only: true,
        });
        self.arena.alloc(TypeKind::Table(table))
    }
    fn type_contains_error_suppressed_dynamic_table(&self, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Table(table) => self.is_error_suppressed_dynamic_table(table),
            TypeKind::Union(types) => types
                .iter()
                .any(|ty| self.type_contains_error_suppressed_dynamic_table(*ty)),
            _ => false,
        }
    }
    fn is_error_suppressed_dynamic_table(&self, table: &TableType) -> bool {
        table.name.as_deref() == Some("table")
    }
    fn for_in_dynamic_key_type(&mut self) -> TypeId {
        let primitives = self.primitives();
        let non_nil = self.arena.alloc(TypeKind::Negation(primitives.nil));
        self.union_type(vec![primitives.error, non_nil])
    }
    pub(crate) fn for_in_pairs_dynamic_argument_values(
        &mut self,
        values: &[Expr],
        var_count: usize,
    ) -> Option<Vec<TypeId>> {
        let [Expr::Call { func, args, .. }, ..] = values else {
            return None;
        };
        let Expr::Global { name, .. } = func.as_ref() else {
            return None;
        };
        if name.as_str() != "pairs" {
            return None;
        }
        let arg = args.first()?;
        let arg_ty = self.dfg_type_for_expr(arg);
        if !self.is_dynamic(arg_ty) {
            return None;
        }
        let dynamic_key = self.for_in_dynamic_key_type();
        let any = self.primitives().any;
        let nil = self.primitives().nil;
        Some(
            (0..var_count)
                .map(|index| match index {
                    0 => dynamic_key,
                    1 => any,
                    _ => nil,
                })
                .collect(),
        )
    }
}
