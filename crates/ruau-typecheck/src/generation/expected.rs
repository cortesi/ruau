//! Contextual expected-type extraction for expression generation.

use std::collections::BTreeSet;

use ruau_ast::syntax::{Expr, TableItem, TableItemKind};

use crate::{
    ast_util::ungroup_expr,
    generation::{
        expression::{TypeofTag, merge_expected_table, static_table_item_key},
        state::ExpressionConstraintGenerator,
    },
    member_access,
    subtype::Subtyper,
    type_function::{Reduction, TypeFunctionRuntime},
    types::{PrimitiveType, SingletonType, TableState, TableType, TypeId, TypeKind},
};

// Methods are intentionally distributed across the generation/ modules
// (expected, expression, statement, lower, state) to keep the large
// constraint-generation pass readable. This is the core of type synthesis.
#[allow(clippy::multiple_inherent_impl)]
impl<'a> ExpressionConstraintGenerator<'a> {
    pub(crate) fn expected_table_for_literal(
        &mut self,
        expected: TypeId,
        items: &[TableItem],
    ) -> Option<TableType> {
        let expected = self.arena.follow(expected);
        if let Some(reduced) = self.reduce_expected_type_function(expected) {
            return self.expected_table_for_literal(reduced, items);
        }
        match self.arena.get(expected).clone() {
            TypeKind::Table(table) => Some(table),
            TypeKind::Union(options) => self.expected_union_table_for_literal(&options, items),
            TypeKind::Intersection(options) => {
                self.expected_intersection_table_for_literal(&options, items)
            }
            _ => None,
        }
    }

    pub(crate) fn expected_union_table_for_literal(
        &mut self,
        options: &[TypeId],
        items: &[TableItem],
    ) -> Option<TableType> {
        let candidates = options
            .iter()
            .copied()
            .filter_map(|option| self.expected_table_candidate_for_literal(option, items))
            .collect::<Vec<_>>();

        if let Some((property, singleton)) = self.literal_discriminator(items) {
            let matching = candidates
                .iter()
                .filter(|table| {
                    self.table_property_type_may_be_singleton(table, &property, &singleton)
                })
                .cloned()
                .collect::<Vec<_>>();
            if let [table] = matching.as_slice() {
                return Some(table.clone());
            }
        }

        match candidates.as_slice() {
            [] => None,
            [table] => Some(table.clone()),
            _ => self.best_expected_table_candidate(candidates, items),
        }
    }

    pub(crate) fn expected_table_candidate_for_literal(
        &mut self,
        expected: TypeId,
        items: &[TableItem],
    ) -> Option<TableType> {
        let expected = self.arena.follow(expected);
        if let Some(reduced) = self.reduce_expected_type_function(expected) {
            return self.expected_table_candidate_for_literal(reduced, items);
        }
        match self.arena.get(expected).clone() {
            TypeKind::Table(table) => Some(table),
            TypeKind::Intersection(options) => {
                self.expected_intersection_table_for_literal(&options, items)
            }
            TypeKind::Union(options) => self.expected_union_table_for_literal(&options, items),
            TypeKind::Never | TypeKind::Primitive(PrimitiveType::Nil) => None,
            _ => None,
        }
    }

    fn reduce_expected_type_function(&mut self, expected: TypeId) -> Option<TypeId> {
        let TypeKind::TypeFunctionInstance { name, arguments } = self.arena.get(expected).clone()
        else {
            return None;
        };
        match TypeFunctionRuntime::new().reduce_allocating(self.arena, &name, &arguments) {
            Reduction::Reduced(reduced) if self.arena.follow(reduced) != expected => {
                Some(self.arena.follow(reduced))
            }
            Reduction::Reduced(_) | Reduction::Pending => None,
        }
    }

    pub(crate) fn expected_intersection_table_for_literal(
        &mut self,
        options: &[TypeId],
        items: &[TableItem],
    ) -> Option<TableType> {
        let mut merged = TableType::new(TableState::Sealed);
        let mut saw_table = false;
        for option in options {
            let option = self.arena.follow(*option);
            match self.arena.get(option).clone() {
                TypeKind::Table(table) => {
                    saw_table = true;
                    merge_expected_table(&mut merged, table)?;
                }
                TypeKind::Union(options) => {
                    saw_table = true;
                    let table = self.expected_union_table_for_literal(&options, items)?;
                    merge_expected_table(&mut merged, table)?;
                }
                TypeKind::Never => {}
                _ => return None,
            }
        }
        saw_table.then_some(merged)
    }

    pub(crate) fn best_expected_table_candidate(
        &mut self,
        candidates: Vec<TableType>,
        items: &[TableItem],
    ) -> Option<TableType> {
        let mut best: Option<(usize, TableType)> = None;
        let mut tied = false;
        for candidate in candidates {
            let score = self.table_literal_match_score(&candidate, items);
            match &best {
                None => {
                    best = Some((score, candidate));
                    tied = false;
                }
                Some((best_score, _)) if score > *best_score => {
                    best = Some((score, candidate));
                    tied = false;
                }
                Some((best_score, _)) if score == *best_score => tied = true,
                Some(_) => {}
            }
        }
        match best {
            Some((score, table)) if score > 0 && !tied => Some(table),
            _ => None,
        }
    }

    pub(crate) fn table_literal_match_score(
        &mut self,
        table: &TableType,
        items: &[TableItem],
    ) -> usize {
        let mut score = 0;
        for item in items {
            if let Some(key) = static_table_item_key(item) {
                if let Some(property) = table.properties.get(&key) {
                    score += 2 + self.expected_literal_value_score(property.ty, &item.value);
                } else if let Some(indexer) = &table.indexer
                    && self.indexer_admits_item_key(item, indexer.key)
                {
                    score += 1 + self.expected_literal_value_score(indexer.value, &item.value);
                }
            } else if matches!(item.kind, TableItemKind::Item | TableItemKind::General)
                && let Some(indexer) = &table.indexer
                && self.indexer_admits_item_key(item, indexer.key)
            {
                score += 1 + self.expected_literal_value_score(indexer.value, &item.value);
            }
        }
        score
    }

    /// Whether a candidate table's indexer whose key type is `indexer_key` can
    /// hold `item`'s implied key. Array-style items use numeric keys and record
    /// fields use string keys, so a string-keyed map (`{ [string]: V }`) must
    /// not score against a positional item and a number-keyed array
    /// (`{ V }`) must not score against a record field. Without this guard the
    /// two table arms of a union such as `JsonValue` score identically, the
    /// candidates tie, `best_expected_table_candidate` discards both, and the
    /// literal's element/field is inferred without its expected (recursive)
    /// union — which then trips the later invariant element check. Items with a
    /// key type we cannot determine statically are left unconstrained.
    fn indexer_admits_item_key(&self, item: &TableItem, indexer_key: TypeId) -> bool {
        let Some(item_key) = self.literal_item_key_type(item) else {
            return true;
        };
        Subtyper::new(self.arena)
            .is_subtype(item_key, indexer_key)
            .is_ok()
    }

    /// The statically known key type of a table-literal item, or `None` when the
    /// key is dynamic. Array-style items index by `number`; record fields and
    /// string-keyed `[expr]` items index by `string`; numeric `[expr]` items
    /// index by `number`.
    fn literal_item_key_type(&self, item: &TableItem) -> Option<TypeId> {
        match item.kind {
            TableItemKind::Item => Some(self.primitives().number),
            TableItemKind::Record => Some(self.primitives().string),
            TableItemKind::General => match item.key.as_ref().map(ungroup_expr) {
                Some(Expr::String { .. }) => Some(self.primitives().string),
                Some(Expr::Number { .. } | Expr::Integer { .. }) => Some(self.primitives().number),
                Some(Expr::Bool { .. }) => Some(self.primitives().boolean),
                _ => None,
            },
        }
    }

    pub(crate) fn expected_literal_value_score(&mut self, expected: TypeId, expr: &Expr) -> usize {
        match expr {
            Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
                self.expected_literal_value_score(expected, expr)
            }
            Expr::Function {
                args,
                self_arg,
                vararg,
                ..
            } => self
                .expected_function_for_function_literal(
                    expected,
                    args.len() + usize::from(self_arg.is_some()),
                    *vararg,
                )
                .map_or(0, |_| 4),
            Expr::Table { items, .. } => self
                .expected_table_for_literal(expected, items)
                .map_or(0, |table| 3 + self.table_literal_match_score(&table, items)),
            Expr::String { value, .. }
                if self.type_may_be_singleton(expected, &SingletonType::String(value.clone())) =>
            {
                2
            }
            Expr::Bool { value, .. }
                if self.type_may_be_singleton(expected, &SingletonType::Boolean(*value)) =>
            {
                2
            }
            Expr::Number { .. } | Expr::Integer { .. }
                if self.typeof_option_matches(
                    expected,
                    &TypeofTag::Primitive(PrimitiveType::Number),
                ) =>
            {
                2
            }
            Expr::Nil { .. } if self.type_accepts_nil(expected, &mut BTreeSet::new()) => 2,
            _ => 0,
        }
    }

    pub(crate) fn table_property_type_may_be_singleton(
        &self,
        table: &TableType,
        property: &str,
        target: &SingletonType,
    ) -> bool {
        table
            .properties
            .get(property)
            .is_some_and(|property| self.type_may_be_singleton(property.ty, target))
    }

    pub(crate) fn expected_function_for_literal(&mut self, expected: TypeId) -> Option<TypeId> {
        let mut candidates = Vec::new();
        self.collect_expected_function_candidates(expected, &mut BTreeSet::new(), &mut candidates);
        match candidates.as_slice() {
            [function] => Some(*function),
            _ => None,
        }
    }

    pub(crate) fn expected_function_for_function_literal(
        &mut self,
        expected: TypeId,
        parameter_count: usize,
        has_vararg: bool,
    ) -> Option<TypeId> {
        let mut candidates = Vec::new();
        self.collect_expected_function_candidates(expected, &mut BTreeSet::new(), &mut candidates);
        match candidates.as_slice() {
            [] => None,
            [function] => Some(*function),
            _ => self.best_expected_function_for_literal_arity(
                candidates.as_slice(),
                parameter_count,
                has_vararg,
            ),
        }
    }

    fn best_expected_function_for_literal_arity(
        &self,
        candidates: &[TypeId],
        parameter_count: usize,
        has_vararg: bool,
    ) -> Option<TypeId> {
        let mut best = None;
        for candidate in candidates {
            let Some(score) =
                self.expected_function_literal_arity_score(*candidate, parameter_count, has_vararg)
            else {
                continue;
            };
            if best.is_none_or(|(best_score, _)| score > best_score) {
                best = Some((score, *candidate));
            }
        }
        best.map(|(_, candidate)| candidate)
    }

    fn expected_function_literal_arity_score(
        &self,
        candidate: TypeId,
        parameter_count: usize,
        has_vararg: bool,
    ) -> Option<usize> {
        let TypeKind::Function(function) = self.arena.get(self.arena.follow(candidate)) else {
            return None;
        };
        let parameters = self.arena.normalize_pack(function.arguments);
        let required = required_function_parameter_count(self.arena, &parameters.types);
        if has_vararg {
            return (parameters.tail.is_some() || parameter_count <= parameters.types.len())
                .then_some(parameters.types.len());
        }
        (parameter_count >= required
            && (parameter_count <= parameters.types.len() || parameters.tail.is_some()))
        .then(|| {
            let fixed_score = parameters.types.len().min(parameter_count);
            let exact_bonus =
                usize::from(parameters.tail.is_none() && parameters.types.len() == parameter_count);
            fixed_score * 2 + exact_bonus
        })
    }

    fn collect_expected_function_candidates(
        &mut self,
        expected: TypeId,
        seen: &mut BTreeSet<TypeId>,
        candidates: &mut Vec<TypeId>,
    ) {
        let expected = self.arena.follow(expected);
        if !seen.insert(expected) {
            return;
        }
        if let Some(reduced) = self.reduce_expected_type_function(expected) {
            self.collect_expected_function_candidates(reduced, seen, candidates);
            return;
        }
        match self.arena.get(expected).clone() {
            TypeKind::Function(_) => candidates.push(expected),
            TypeKind::Union(options) => {
                for option in options {
                    self.collect_expected_function_candidates(option, seen, candidates);
                }
            }
            TypeKind::Never | TypeKind::Primitive(PrimitiveType::Nil) => {}
            _ => {}
        }
    }
}

fn required_function_parameter_count(arena: &crate::types::Arena, types: &[TypeId]) -> usize {
    types
        .iter()
        .rposition(|ty| !member_access::type_accepts_nil_for_arity(arena, *ty))
        .map_or(0, |index| index + 1)
}
