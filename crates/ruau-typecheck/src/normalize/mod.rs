//! Type normalization and simplification.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    type_function::{Reduction, TypeFunctionRuntime},
    types::{
        Arena, FunctionType, PrimitiveType, SingletonType, TableIndexer, TableProperty, TableState,
        TableType, TypeId, TypeKind, TypePackId, TypePackKind, alloc_top_function_type,
        extern_is_subtype, is_top_function_type,
    },
};

/// Arena-mutating normalizer.
pub struct Normalizer<'a> {
    arena: &'a mut Arena,
    visiting: BTreeSet<TypeId>,
    combining_table_properties: BTreeSet<(TypeId, TypeId)>,
    expand_extern_negations: bool,
    symbolic_extern_negations: BTreeSet<TypeId>,
    remaining_fuel: Option<usize>,
    hit_limit: bool,
}

impl<'a> Normalizer<'a> {
    /// Creates a normalizer over a mutable type arena.
    pub fn new(arena: &'a mut Arena) -> Self {
        Self {
            arena,
            visiting: BTreeSet::new(),
            combining_table_properties: BTreeSet::new(),
            expand_extern_negations: false,
            symbolic_extern_negations: BTreeSet::new(),
            remaining_fuel: None,
            hit_limit: false,
        }
    }

    /// Test helper that flips on extern/userdata negation expansion.
    #[cfg(any())]
    pub fn with_extern_negation_expansion(mut self) -> Self {
        self.expand_extern_negations = true;
        self
    }

    /// Creates a normalizer that reports failure after the supplied operation
    /// budget is exhausted. Used by the fuel-budget tests in
    /// `normalize_tests.rs`.
    #[cfg(any())]
    pub fn with_fuel_limit(mut self, fuel: usize) -> Self {
        self.remaining_fuel = Some(fuel);
        self
    }

    /// Returns a simplified type, or `None` if the configured fuel limit was
    /// exhausted before simplification completed. Used by the fuel-budget
    /// tests in `normalize_tests.rs`.
    #[cfg(any())]
    pub fn try_simplify_type(&mut self, id: TypeId) -> Option<TypeId> {
        self.hit_limit = false;
        let result = self.simplify_type(id);
        (!self.hit_limit).then_some(result)
    }

    /// Returns a simplified type handle, allocating canonicalized composite
    /// nodes when needed.
    pub fn simplify_type(&mut self, id: TypeId) -> TypeId {
        let id = self.arena.follow(id);
        if !self.consume_fuel() {
            return id;
        }
        if !self.visiting.insert(id) {
            return id;
        }
        let result = self.simplify_type_inner(id);
        self.visiting.remove(&id);
        result
    }

    fn simplify_type_inner(&mut self, id: TypeId) -> TypeId {
        match self.arena.get(id).clone() {
            TypeKind::Function(function) => self.simplify_function(id, function),
            TypeKind::Table(table) => self.simplify_table(id, table),
            TypeKind::Metatable {
                table,
                metatable,
                name,
            } => {
                let simplified_table = self.simplify_type(table);
                let simplified_metatable = self.simplify_type(metatable);
                if simplified_table == table && simplified_metatable == metatable {
                    return id;
                }
                self.arena.alloc(TypeKind::Metatable {
                    table: simplified_table,
                    metatable: simplified_metatable,
                    name,
                })
            }
            TypeKind::TypeFunctionInstance { name, arguments } => {
                let simplified_arguments = arguments
                    .iter()
                    .map(|ty| self.simplify_type(*ty))
                    .collect::<Vec<_>>();
                match TypeFunctionRuntime::new().reduce_allocating(
                    self.arena,
                    &name,
                    &simplified_arguments,
                ) {
                    Reduction::Reduced(reduced) if reduced != id => self.simplify_type(reduced),
                    Reduction::Reduced(_) | Reduction::Pending => {
                        if simplified_arguments == arguments {
                            return id;
                        }
                        self.arena.alloc(TypeKind::TypeFunctionInstance {
                            name,
                            arguments: simplified_arguments,
                        })
                    }
                }
            }
            TypeKind::Union(options) => {
                if let Some(seed) = self.simplify_recursive_union_seed(id, &options) {
                    return seed;
                }
                if options.contains(&id)
                    && options
                        .iter()
                        .filter(|option| **option != id)
                        .all(|option| self.simplify_type(*option) == self.arena.primitives().never)
                {
                    self.arena.primitives().never
                } else {
                    self.simplify_union(options)
                }
            }
            TypeKind::Intersection(options) => {
                if let Some(seed) = self.simplify_recursive_intersection_seed(id, &options) {
                    seed
                } else {
                    self.simplify_intersection(options)
                }
            }
            TypeKind::Negation(ty) => self.simplify_negation(ty),
            TypeKind::Bound(bound) => self.simplify_type(bound),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => id,
        }
    }

    /// Returns a simplified type-pack handle.
    pub fn simplify_pack(&mut self, id: TypePackId) -> TypePackId {
        let id = self.arena.follow_pack(id);
        if !self.consume_fuel() {
            return id;
        }
        match self.arena.get_pack(id).clone() {
            TypePackKind::List { types, tail } => {
                let simplified_types: Vec<TypeId> =
                    types.iter().map(|ty| self.simplify_type(*ty)).collect();
                let simplified_tail = tail.map(|tail| self.simplify_pack(tail));
                if simplified_types == types && simplified_tail == tail {
                    return id;
                }
                self.arena.alloc_pack(TypePackKind::List {
                    types: simplified_types,
                    tail: simplified_tail,
                })
            }
            TypePackKind::Variadic { ty } => {
                let simplified = self.simplify_type(ty);
                if simplified == ty {
                    return id;
                }
                self.arena
                    .alloc_pack(TypePackKind::Variadic { ty: simplified })
            }
            TypePackKind::Bound(bound) => self.simplify_pack(bound),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => id,
        }
    }

    fn consume_fuel(&mut self) -> bool {
        let Some(remaining) = &mut self.remaining_fuel else {
            return true;
        };
        if *remaining == 0 {
            self.hit_limit = true;
            return false;
        }
        *remaining -= 1;
        true
    }

    fn simplify_function(&mut self, id: TypeId, mut function: FunctionType) -> TypeId {
        let arguments = self.simplify_pack(function.arguments);
        let returns = self.simplify_pack(function.returns);
        if arguments == function.arguments && returns == function.returns {
            return id;
        }
        function.arguments = arguments;
        function.returns = returns;
        self.arena.alloc(TypeKind::Function(function))
    }

    fn simplify_recursive_union_seed(&mut self, id: TypeId, options: &[TypeId]) -> Option<TypeId> {
        let mut seeds = Vec::new();
        let mut saw_cycle = false;
        for option in options {
            if self.references_type(*option, id, &mut BTreeSet::new(), &mut BTreeSet::new()) {
                saw_cycle = true;
            } else {
                seeds.push(*option);
            }
        }
        (saw_cycle && !seeds.is_empty()).then(|| self.simplify_union(seeds))
    }

    fn simplify_recursive_intersection_seed(
        &mut self,
        id: TypeId,
        options: &[TypeId],
    ) -> Option<TypeId> {
        for (index, option) in options.iter().enumerate() {
            let TypeKind::Union(union_options) = self.arena.get(*option).clone() else {
                continue;
            };
            if !union_options.iter().any(|union_option| {
                self.references_type(
                    *union_option,
                    id,
                    &mut BTreeSet::new(),
                    &mut BTreeSet::new(),
                )
            }) {
                continue;
            }
            let mut members: Vec<_> = options
                .iter()
                .enumerate()
                .filter_map(|(candidate, ty)| (candidate != index).then_some(*ty))
                .filter(|ty| !matches!(self.arena.get(*ty), TypeKind::Any))
                .collect();
            members.extend(union_options.into_iter().filter(|union_option| {
                !self.references_type(
                    *union_option,
                    id,
                    &mut BTreeSet::new(),
                    &mut BTreeSet::new(),
                )
            }));
            return Some(self.simplify_intersection(members));
        }
        None
    }

    fn references_type(
        &self,
        ty: TypeId,
        target: TypeId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if ty == target {
            return true;
        }
        if !seen_types.insert(ty) {
            return false;
        }
        match self.arena.get(ty) {
            TypeKind::Union(options) | TypeKind::Intersection(options) => options
                .iter()
                .any(|option| self.references_type(*option, target, seen_types, seen_packs)),
            TypeKind::Negation(inner) => {
                self.references_type(*inner, target, seen_types, seen_packs)
            }
            TypeKind::Function(function) => {
                self.pack_references_type(function.arguments, target, seen_types, seen_packs)
                    || self.pack_references_type(function.returns, target, seen_types, seen_packs)
            }
            TypeKind::Table(table) => {
                table.properties.values().any(|property| {
                    self.references_type(property.ty, target, seen_types, seen_packs)
                }) || table.indexer.as_ref().is_some_and(|indexer| {
                    self.references_type(indexer.key, target, seen_types, seen_packs)
                        || self.references_type(indexer.value, target, seen_types, seen_packs)
                })
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.references_type(*table, target, seen_types, seen_packs)
                    || self.references_type(*metatable, target, seen_types, seen_packs)
            }
            TypeKind::TypeFunctionInstance { arguments, .. } => arguments
                .iter()
                .any(|argument| self.references_type(*argument, target, seen_types, seen_packs)),
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }

    fn pack_references_type(
        &self,
        pack: TypePackId,
        target: TypeId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack) {
            TypePackKind::List { types, tail } => {
                types
                    .iter()
                    .any(|ty| self.references_type(*ty, target, seen_types, seen_packs))
                    || tail.is_some_and(|tail| {
                        self.pack_references_type(tail, target, seen_types, seen_packs)
                    })
            }
            TypePackKind::Variadic { ty } => {
                self.references_type(*ty, target, seen_types, seen_packs)
            }
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }

    fn simplify_table(&mut self, id: TypeId, mut table: TableType) -> TypeId {
        let original = table.clone();
        table.instantiated_type_params = table
            .instantiated_type_params
            .into_iter()
            .map(|ty| self.simplify_type(ty))
            .collect();
        table.properties = table
            .properties
            .into_iter()
            .map(|(name, property)| {
                (
                    name,
                    TableProperty {
                        ty: self.simplify_type(property.ty),
                        write_ty: property.write_ty.map(|ty| self.simplify_type(ty)),
                        location: property.location,
                        documentation_symbol: property.documentation_symbol,
                        read_only: property.read_only,
                        write_only: property.write_only,
                        deprecated: property.deprecated,
                    },
                )
            })
            .collect();
        if table.properties.values().any(|property| {
            matches!(
                self.arena.get(self.arena.follow(property.ty)),
                TypeKind::Never
            )
        }) {
            return self.arena.primitives().never;
        }
        table.indexer = table.indexer.map(|indexer| TableIndexer {
            key: self.simplify_type(indexer.key),
            value: self.simplify_type(indexer.value),
            read_only: indexer.read_only,
        });
        if table == original {
            return id;
        }
        self.arena.alloc(TypeKind::Table(table))
    }

    fn simplify_union(&mut self, options: Vec<TypeId>) -> TypeId {
        let mut options = options;
        remove_duplicate_types(&mut options);
        let mut flattened = Vec::new();
        for option in options {
            let option = self.simplify_type(option);
            match self.arena.get(option).clone() {
                TypeKind::Any | TypeKind::Unknown => return option,
                TypeKind::Union(nested) => flattened.extend(nested),
                TypeKind::Never => {}
                _ => flattened.push(option),
            }
        }

        remove_duplicate_types(&mut flattened);
        remove_duplicate_type_shapes(self.arena, &mut flattened);
        collapse_functions_covered_by_top_function(self.arena, &mut flattened);
        collapse_tables_covered_by_top_table(self.arena, &mut flattened);
        collapse_externs_covered_by_parents(self.arena, &mut flattened);
        if negated_disjoint_primitives_cover_unknown(self.arena, &flattened) {
            return self.arena.primitives().unknown;
        }
        collapse_covered_negated_singletons(self.arena, &mut flattened);
        collapse_singletons_covered_by_primitives(self.arena, &mut flattened);
        collapse_boolean_singletons(self.arena, &mut flattened);

        match flattened.as_slice() {
            [] => self.arena.primitives().never,
            [only] => *only,
            _ => self.arena.alloc(TypeKind::Union(flattened)),
        }
    }

    fn simplify_intersection(&mut self, options: Vec<TypeId>) -> TypeId {
        let mut options = options;
        remove_duplicate_types(&mut options);
        let mut flattened = Vec::new();
        let mut saw_any = false;
        let mut saw_unknown = false;
        let mut saw_error = false;
        for option in options {
            let option = self.simplify_type(option);
            match self.arena.get(option).clone() {
                TypeKind::Never => return option,
                TypeKind::Intersection(nested) => flattened.extend(nested),
                TypeKind::Any => saw_any = true,
                TypeKind::Unknown => saw_unknown = true,
                TypeKind::Error => saw_error = true,
                _ => flattened.push(option),
            }
        }

        remove_duplicate_types(&mut flattened);
        remove_duplicate_type_shapes(self.arena, &mut flattened);
        let mut negations = Vec::new();
        flattened.retain(|ty| match self.arena.get(*ty) {
            TypeKind::Negation(target) => {
                negations.push((*ty, *target));
                false
            }
            _ => true,
        });
        collapse_negated_extern_intersections(self.arena, &mut negations);
        if let Some((index, options)) =
            flattened
                .iter()
                .enumerate()
                .find_map(|(index, ty)| match self.arena.get(*ty) {
                    TypeKind::Union(options) => Some((index, options.clone())),
                    _ => None,
                })
        {
            let rest: Vec<_> = flattened
                .iter()
                .enumerate()
                .filter_map(|(candidate, ty)| (candidate != index).then_some(*ty))
                .collect();
            if !rest.iter().any(|ty| is_indeterminate(self.arena, *ty)) {
                let mut union_options = Vec::new();
                let negation_ids: Vec<_> = negations.iter().map(|(id, _)| *id).collect();
                for option in options {
                    let mut members = rest.clone();
                    members.push(option);
                    members.extend(negation_ids.iter().copied());
                    let distributed = self.simplify_intersection(members);
                    if distributed != self.arena.primitives().never {
                        union_options.push(distributed);
                    }
                }
                return self.simplify_union(union_options);
            }
        }
        for (negation_id, target) in negations {
            match self.apply_negation_to_intersection(flattened, target) {
                NegationApplication::Never => return self.arena.primitives().never,
                NegationApplication::Applied(types) => flattened = types,
                NegationApplication::Keep(mut types) => {
                    types.push(negation_id);
                    flattened = types;
                }
            }
        }
        if saw_error {
            return self.simplify_error_intersection(flattened, saw_any, saw_unknown);
        }
        if saw_any {
            return self.simplify_any_intersection(flattened, saw_unknown);
        }
        if saw_unknown && flattened.is_empty() {
            return self.arena.primitives().unknown;
        }
        self.simplify_metatable_intersections(&mut flattened);
        self.simplify_table_intersections(&mut flattened);
        self.simplify_function_intersections(&mut flattened);
        if !simplify_extern_intersections(self.arena, &mut flattened) {
            return self.arena.primitives().never;
        }
        if flattened
            .iter()
            .any(|ty| matches!(self.arena.get(*ty), TypeKind::Never))
        {
            return self.arena.primitives().never;
        }
        if incompatible_primitive_intersection(self.arena, &flattened)
            || incompatible_singleton_intersection(self.arena, &flattened)
            || incompatible_primitive_singleton_intersection(self.arena, &flattened)
            || incompatible_composite_scalar_intersection(self.arena, &flattened)
            || incompatible_extern_runtime_intersection(self.arena, &flattened)
            || incompatible_function_table_intersection(self.arena, &flattened)
        {
            return self.arena.primitives().never;
        }
        narrow_singletons_by_primitives(self.arena, &mut flattened);
        order_intersection_options(self.arena, &mut flattened);

        match flattened.as_slice() {
            [] => self.arena.primitives().unknown,
            [only] => *only,
            _ => self.arena.alloc(TypeKind::Intersection(flattened)),
        }
    }

    fn simplify_table_intersections(&mut self, flattened: &mut Vec<TypeId>) {
        loop {
            let Some((left_index, right_index, combined)) =
                self.find_simplifiable_table_intersection(flattened)
            else {
                return;
            };
            flattened.remove(right_index);
            flattened[left_index] = combined;
            remove_duplicate_types(flattened);
            remove_duplicate_type_shapes(self.arena, flattened);
        }
    }

    fn find_simplifiable_table_intersection(
        &mut self,
        flattened: &[TypeId],
    ) -> Option<(usize, usize, TypeId)> {
        for left_index in 0..flattened.len() {
            let TypeKind::Table(left) = self.arena.get(flattened[left_index]).clone() else {
                continue;
            };
            for (right_index, right_ty) in flattened.iter().enumerate().skip(left_index + 1) {
                let TypeKind::Table(right) = self.arena.get(*right_ty).clone() else {
                    continue;
                };
                if let Some(combined) = self.combine_table_intersection(left.clone(), right) {
                    return Some((left_index, right_index, combined));
                }
            }
        }
        None
    }

    fn simplify_metatable_intersections(&mut self, flattened: &mut Vec<TypeId>) {
        loop {
            let Some((left_index, right_index, combined)) =
                self.find_simplifiable_metatable_intersection(flattened)
            else {
                return;
            };
            flattened.remove(right_index);
            flattened[left_index] = combined;
            remove_duplicate_types(flattened);
            remove_duplicate_type_shapes(self.arena, flattened);
        }
    }

    fn find_simplifiable_metatable_intersection(
        &mut self,
        flattened: &[TypeId],
    ) -> Option<(usize, usize, TypeId)> {
        for left_index in 0..flattened.len() {
            let TypeKind::Metatable {
                table: left_table,
                metatable: left_metatable,
                name: left_name,
            } = self.arena.get(flattened[left_index]).clone()
            else {
                continue;
            };
            for (right_index, right_ty) in flattened.iter().enumerate().skip(left_index + 1) {
                let TypeKind::Metatable {
                    table: right_table,
                    metatable: right_metatable,
                    name: right_name,
                } = self.arena.get(*right_ty).clone()
                else {
                    continue;
                };
                let table = self
                    .arena
                    .alloc(TypeKind::Intersection(vec![left_table, right_table]));
                let table = self.simplify_type(table);
                let metatable = self.arena.alloc(TypeKind::Intersection(vec![
                    left_metatable,
                    right_metatable,
                ]));
                let metatable = self.simplify_type(metatable);
                let combined = if table == self.arena.primitives().never
                    || metatable == self.arena.primitives().never
                {
                    self.arena.primitives().never
                } else {
                    self.arena.alloc(TypeKind::Metatable {
                        table,
                        metatable,
                        name: (left_name == right_name).then_some(left_name).flatten(),
                    })
                };
                return Some((left_index, right_index, combined));
            }
        }
        None
    }

    fn simplify_function_intersections(&self, flattened: &mut Vec<TypeId>) {
        let has_top_function = flattened.iter().any(|ty| {
            matches!(
                self.arena.get(*ty),
                TypeKind::Function(function) if is_top_function_type(self.arena, function)
            )
        });
        if !has_top_function {
            return;
        }

        let has_specific_function = flattened.iter().any(|ty| {
            matches!(
                self.arena.get(*ty),
                TypeKind::Function(function) if !is_top_function_type(self.arena, function)
            )
        });
        if !has_specific_function {
            return;
        }

        flattened.retain(|ty| {
            !matches!(
                self.arena.get(*ty),
                TypeKind::Function(function) if is_top_function_type(self.arena, function)
            )
        });
        remove_duplicate_types(flattened);
        remove_duplicate_type_shapes(self.arena, flattened);
    }

    fn combine_table_intersection(&mut self, left: TableType, right: TableType) -> Option<TypeId> {
        if is_top_table_type(&left) {
            return Some(self.arena.alloc(TypeKind::Table(right)));
        }
        if is_top_table_type(&right) {
            return Some(self.arena.alloc(TypeKind::Table(left)));
        }
        if left.state != right.state
            || left.indexer.is_some()
            || right.indexer.is_some()
            || left.instantiated_type_params != right.instantiated_type_params
            || !left
                .properties
                .keys()
                .chain(right.properties.keys())
                .all(|name| is_identifier_property_name(name))
        {
            return None;
        }

        let left_keys = left.properties.keys().collect::<BTreeSet<_>>();
        let right_keys = right.properties.keys().collect::<BTreeSet<_>>();
        if left_keys.is_disjoint(&right_keys) {
            let mut combined = left;
            combined.properties.extend(right.properties);
            return Some(self.arena.alloc(TypeKind::Table(combined)));
        }

        if self.table_is_unknown_property_subset_of(&right, &left) {
            return Some(self.arena.alloc(TypeKind::Table(left)));
        }
        if self.table_is_unknown_property_subset_of(&left, &right) {
            return Some(self.arena.alloc(TypeKind::Table(right)));
        }
        if left_keys != right_keys {
            return self.collapse_unchanged_single_overlap(&left, &right, &left_keys, &right_keys);
        }
        if let Some(combined) = self.combine_overlapping_table_intersection(left, right) {
            let combined = self.arena.alloc(TypeKind::Table(combined));
            return Some(self.simplify_type(combined));
        }
        None
    }

    fn collapse_unchanged_single_overlap(
        &mut self,
        left: &TableType,
        right: &TableType,
        left_keys: &BTreeSet<&String>,
        right_keys: &BTreeSet<&String>,
    ) -> Option<TypeId> {
        let overlapping_keys = left_keys.intersection(right_keys).collect::<Vec<_>>();
        let [name] = overlapping_keys.as_slice() else {
            return None;
        };
        let left_property = left.properties.get(**name)?;
        let right_property = right.properties.get(**name)?;
        let combined = self.combine_table_properties(left_property, right_property)?;
        if left_keys.is_superset(right_keys)
            && table_property_matches(self, &combined, left_property)
        {
            return Some(self.arena.alloc(TypeKind::Table(left.clone())));
        }
        if right_keys.is_superset(left_keys)
            && table_property_matches(self, &combined, right_property)
        {
            return Some(self.arena.alloc(TypeKind::Table(right.clone())));
        }
        None
    }

    fn combine_overlapping_table_intersection(
        &mut self,
        left: TableType,
        right: TableType,
    ) -> Option<TableType> {
        let mut combined = left;
        let mut changed = false;
        for (name, right_property) in right.properties {
            let Some(left_property) = combined.properties.get(&name).cloned() else {
                combined.properties.insert(name, right_property);
                changed = true;
                continue;
            };
            combined.properties.insert(
                name,
                self.combine_table_properties(&left_property, &right_property)?,
            );
            changed = true;
        }
        changed.then_some(combined)
    }

    fn combine_table_properties(
        &mut self,
        left: &TableProperty,
        right: &TableProperty,
    ) -> Option<TableProperty> {
        if left.deprecated != right.deprecated {
            return None;
        }
        let ty = self.combine_table_property_intersection(left.ty, right.ty)?;
        let (read_only, write_only) =
            if left.read_only == right.read_only && left.write_only == right.write_only {
                (left.read_only, left.write_only)
            } else if ty == self.arena.follow(left.ty) && ty == self.arena.follow(right.ty) {
                intersect_property_capabilities(left, right)
            } else {
                return None;
            };
        Some(TableProperty {
            ty,
            write_ty: None,
            location: left.location.or(right.location),
            documentation_symbol: left
                .documentation_symbol
                .clone()
                .or_else(|| right.documentation_symbol.clone()),
            read_only,
            write_only,
            deprecated: left.deprecated,
        })
    }

    fn combine_table_property_intersection(
        &mut self,
        left: TypeId,
        right: TypeId,
    ) -> Option<TypeId> {
        let left = self.arena.follow(left);
        let right = self.arena.follow(right);
        if left == right {
            return Some(left);
        }
        let key = if left <= right {
            (left, right)
        } else {
            (right, left)
        };
        if !self.combining_table_properties.insert(key) {
            return Some(left);
        }
        if self.visiting.contains(&left) || self.visiting.contains(&right) {
            self.combining_table_properties.remove(&key);
            return Some(left);
        }
        let TypeKind::Table(left_table) = self.arena.get(left).clone() else {
            self.combining_table_properties.remove(&key);
            let intersection = self.arena.alloc(TypeKind::Intersection(vec![left, right]));
            return Some(self.simplify_type(intersection));
        };
        let TypeKind::Table(right_table) = self.arena.get(right).clone() else {
            self.combining_table_properties.remove(&key);
            let intersection = self.arena.alloc(TypeKind::Intersection(vec![left, right]));
            return Some(self.simplify_type(intersection));
        };
        let combined = self.combine_table_intersection(left_table, right_table);
        self.combining_table_properties.remove(&key);
        combined
    }

    fn table_is_unknown_property_subset_of(
        &self,
        subset: &TableType,
        superset: &TableType,
    ) -> bool {
        !subset.properties.is_empty()
            && subset.properties.iter().all(|(name, property)| {
                let Some(superset_property) = superset.properties.get(name) else {
                    return false;
                };
                property.deprecated == superset_property.deprecated
                    && matches!(
                        self.arena.get(self.arena.follow(property.ty)),
                        TypeKind::Unknown
                    )
            })
    }

    fn apply_negation_to_intersection(
        &mut self,
        flattened: Vec<TypeId>,
        target: TypeId,
    ) -> NegationApplication {
        if flattened.is_empty() {
            return NegationApplication::Keep(flattened);
        }
        let target = self.simplify_type(target);
        if let TypeKind::Union(options) = self.arena.get(target).clone() {
            let mut current = flattened;
            for option in options {
                match self.apply_negation_to_intersection(current, option) {
                    NegationApplication::Never => return NegationApplication::Never,
                    NegationApplication::Applied(types) | NegationApplication::Keep(types) => {
                        current = types;
                    }
                }
            }
            return NegationApplication::Applied(current);
        }

        let mut next = Vec::new();
        let mut keep = false;
        for ty in flattened {
            match self.subtract_negated_type(ty, target) {
                TypeSubtraction::Never => return NegationApplication::Never,
                TypeSubtraction::Disjoint(id) | TypeSubtraction::Applied(id) => next.push(id),
                TypeSubtraction::Keep(id) => {
                    next.push(id);
                    keep = true;
                }
            }
        }
        remove_duplicate_types(&mut next);
        if keep {
            NegationApplication::Keep(next)
        } else {
            NegationApplication::Applied(next)
        }
    }

    fn subtract_negated_type(&mut self, ty: TypeId, target: TypeId) -> TypeSubtraction {
        let ty = self.arena.follow(ty);
        let target = self.arena.follow(target);
        match (self.arena.get(ty).clone(), self.arena.get(target).clone()) {
            (_, TypeKind::Never) => TypeSubtraction::Disjoint(ty),
            (_, TypeKind::Any | TypeKind::Unknown) => TypeSubtraction::Never,
            (TypeKind::Primitive(left), TypeKind::Primitive(right)) => {
                if left == right {
                    TypeSubtraction::Never
                } else {
                    TypeSubtraction::Disjoint(ty)
                }
            }
            (
                TypeKind::Primitive(PrimitiveType::Boolean),
                TypeKind::Singleton(SingletonType::Boolean(value)),
            ) => {
                let opposite = self
                    .arena
                    .alloc(TypeKind::Singleton(SingletonType::Boolean(!value)));
                TypeSubtraction::Applied(opposite)
            }
            (TypeKind::Primitive(left), TypeKind::Singleton(singleton)) => {
                if left == singleton.primitive() {
                    TypeSubtraction::Keep(ty)
                } else {
                    TypeSubtraction::Disjoint(ty)
                }
            }
            (TypeKind::Singleton(singleton), TypeKind::Primitive(primitive)) => {
                if singleton.primitive() == primitive {
                    TypeSubtraction::Never
                } else {
                    TypeSubtraction::Disjoint(ty)
                }
            }
            (TypeKind::Singleton(left), TypeKind::Singleton(right)) => {
                if left == right {
                    TypeSubtraction::Never
                } else {
                    TypeSubtraction::Disjoint(ty)
                }
            }
            (
                TypeKind::Function(_) | TypeKind::Table(_) | TypeKind::Metatable { .. },
                TypeKind::Primitive(_) | TypeKind::Singleton(_),
            )
            | (
                TypeKind::Primitive(_) | TypeKind::Singleton(_),
                TypeKind::Function(_) | TypeKind::Table(_) | TypeKind::Metatable { .. },
            ) => TypeSubtraction::Disjoint(ty),
            (left, TypeKind::Function(target)) if is_top_function_type(self.arena, &target) => {
                match left {
                    TypeKind::Function(_) => TypeSubtraction::Never,
                    TypeKind::Primitive(_)
                    | TypeKind::Singleton(_)
                    | TypeKind::Table(_)
                    | TypeKind::Metatable { .. } => TypeSubtraction::Disjoint(ty),
                    _ => TypeSubtraction::Keep(ty),
                }
            }
            (left, TypeKind::Table(target)) if is_top_table_type(&target) => match left {
                TypeKind::Table(_) | TypeKind::Metatable { .. } => TypeSubtraction::Never,
                TypeKind::Primitive(_) | TypeKind::Singleton(_) | TypeKind::Function(_) => {
                    TypeSubtraction::Disjoint(ty)
                }
                _ => TypeSubtraction::Keep(ty),
            },
            (
                TypeKind::Extern {
                    name: left,
                    parents: left_parents,
                    ..
                },
                TypeKind::Extern {
                    name: right,
                    parents: right_parents,
                    ..
                },
            ) => {
                if extern_is_subtype(&left, &left_parents, &right) {
                    TypeSubtraction::Never
                } else if extern_is_subtype(&right, &right_parents, &left) {
                    TypeSubtraction::Keep(ty)
                } else {
                    TypeSubtraction::Disjoint(ty)
                }
            }
            _ => TypeSubtraction::Keep(ty),
        }
    }

    fn simplify_error_intersection(
        &mut self,
        mut flattened: Vec<TypeId>,
        saw_any: bool,
        saw_unknown: bool,
    ) -> TypeId {
        if flattened.is_empty() {
            return self.arena.primitives().error;
        }
        if saw_any || saw_unknown || flattened.iter().all(|ty| is_indeterminate(self.arena, *ty)) {
            flattened.push(self.arena.primitives().error);
            remove_duplicate_types(&mut flattened);
            return match flattened.as_slice() {
                [only] => *only,
                _ => self.arena.alloc(TypeKind::Intersection(flattened)),
            };
        }
        self.arena.primitives().error
    }

    fn simplify_any_intersection(
        &mut self,
        mut flattened: Vec<TypeId>,
        saw_unknown: bool,
    ) -> TypeId {
        if flattened.is_empty() {
            return self.arena.primitives().any;
        }
        if saw_unknown {
            flattened.push(self.arena.primitives().any);
            remove_duplicate_types(&mut flattened);
            return match flattened.as_slice() {
                [only] => *only,
                _ => self.arena.alloc(TypeKind::Intersection(flattened)),
            };
        }
        flattened.push(self.arena.primitives().error);
        remove_duplicate_types(&mut flattened);
        self.simplify_union(flattened)
    }

    fn simplify_negation(&mut self, ty: TypeId) -> TypeId {
        let ty = self.arena.follow(ty);
        if let TypeKind::Negation(inner) = self.arena.get(ty).clone() {
            return self.simplify_type(inner);
        }
        if self.expand_extern_negations && self.symbolic_extern_negations.contains(&ty) {
            return self.arena.alloc(TypeKind::Negation(ty));
        }
        if self.expand_extern_negations
            && let Some(complement) = self.extern_negation_complement(ty)
        {
            return complement;
        }

        let ty = self.simplify_type(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Never => self.arena.primitives().unknown,
            TypeKind::Any => self.arena.primitives().unknown,
            TypeKind::Unknown => self.arena.primitives().never,
            TypeKind::Error => self.arena.primitives().error,
            TypeKind::Blocked(_) => ty,
            TypeKind::Primitive(PrimitiveType::Boolean) => self.runtime_non_boolean_type(),
            TypeKind::Function(function) if is_top_function_type(self.arena, &function) => {
                self.runtime_non_function_type()
            }
            TypeKind::Table(table) if is_top_table_type(&table) => self.runtime_non_table_type(),
            _ => self.arena.alloc(TypeKind::Negation(ty)),
        }
    }

    fn extern_negation_complement(&mut self, ty: TypeId) -> Option<TypeId> {
        match self.arena.get(ty).clone() {
            TypeKind::Extern { name, parents, .. } => {
                self.extern_type_complement(ty, name.as_str(), &parents)
            }
            TypeKind::Intersection(options) => self.extern_residual_complement(&options),
            _ => None,
        }
    }

    fn extern_type_complement(
        &mut self,
        target: TypeId,
        name: &str,
        parents: &[String],
    ) -> Option<TypeId> {
        let root_name = extern_complement_root(name, parents)?;
        let non_extern = self.runtime_non_extern_type();
        if name == root_name {
            return Some(non_extern);
        }

        let root = self.arena.alloc(TypeKind::Extern {
            name: root_name.to_owned(),
            parents: Vec::new(),
            properties: BTreeMap::new(),
            indexer: None,
        });
        let symbolic_target = self.arena.alloc(self.arena.get(target).clone());
        self.symbolic_extern_negations.insert(symbolic_target);
        let not_target = self.arena.alloc(TypeKind::Negation(symbolic_target));
        let residual = self
            .arena
            .alloc(TypeKind::Intersection(vec![root, not_target]));
        Some(self.simplify_union(vec![non_extern, residual]))
    }

    fn extern_residual_complement(&mut self, options: &[TypeId]) -> Option<TypeId> {
        let mut root: Option<(String, TypeId)> = None;
        let mut excluded = Vec::new();
        for option in options {
            let option = self.arena.follow(*option);
            match self.arena.get(option).clone() {
                TypeKind::Extern { name, parents, .. } => {
                    let root_name = extern_complement_root(&name, &parents)?;
                    if name != root_name {
                        return None;
                    }
                    root = Some((name, option));
                }
                TypeKind::Negation(target) => {
                    let target = self.arena.follow(target);
                    let TypeKind::Extern { name, parents, .. } = self.arena.get(target) else {
                        return None;
                    };
                    extern_complement_root(name, parents)?;
                    excluded.push(target);
                }
                _ => return None,
            }
        }

        let (root_name, root_ty) = root?;
        let mut members = vec![self.runtime_non_extern_type()];
        members.extend(excluded);
        if members.len() == 1 {
            members.push(root_ty);
        }
        let complement = self.simplify_union(members);
        if root_name == "userdata" {
            Some(complement)
        } else {
            None
        }
    }

    fn runtime_non_boolean_type(&mut self) -> TypeId {
        let primitives = self.arena.primitives();
        let top_function = alloc_top_function_type(self.arena);
        let top_table = alloc_top_table_type(self.arena);
        self.simplify_union(vec![
            primitives.nil,
            primitives.number,
            primitives.string,
            primitives.thread,
            primitives.buffer,
            primitives.vector,
            top_function,
            top_table,
        ])
    }

    fn runtime_non_function_type(&mut self) -> TypeId {
        let primitives = self.arena.primitives();
        let top_table = alloc_top_table_type(self.arena);
        self.simplify_union(vec![
            primitives.nil,
            primitives.boolean,
            primitives.number,
            primitives.string,
            primitives.thread,
            primitives.buffer,
            primitives.vector,
            top_table,
        ])
    }

    fn runtime_non_table_type(&mut self) -> TypeId {
        let primitives = self.arena.primitives();
        let top_function = alloc_top_function_type(self.arena);
        self.simplify_union(vec![
            primitives.nil,
            primitives.boolean,
            primitives.number,
            primitives.string,
            primitives.thread,
            primitives.buffer,
            primitives.vector,
            top_function,
        ])
    }

    fn runtime_non_extern_type(&mut self) -> TypeId {
        let primitives = self.arena.primitives();
        let top_function = alloc_top_function_type(self.arena);
        let top_table = alloc_top_table_type(self.arena);
        self.simplify_union(vec![
            primitives.nil,
            primitives.boolean,
            primitives.number,
            primitives.string,
            primitives.thread,
            primitives.buffer,
            primitives.vector,
            top_function,
            top_table,
        ])
    }
}

enum NegationApplication {
    Never,
    Applied(Vec<TypeId>),
    Keep(Vec<TypeId>),
}

enum TypeSubtraction {
    Never,
    Disjoint(TypeId),
    Applied(TypeId),
    Keep(TypeId),
}

fn remove_duplicate_types(types: &mut Vec<TypeId>) {
    let mut seen = BTreeSet::new();
    types.retain(|ty| seen.insert(*ty));
}

fn intersect_property_capabilities(left: &TableProperty, right: &TableProperty) -> (bool, bool) {
    let can_read = !left.write_only || !right.write_only;
    let can_write = !left.read_only || !right.read_only;
    (can_read && !can_write, can_write && !can_read)
}

fn table_property_matches(
    normalizer: &Normalizer<'_>,
    left: &TableProperty,
    right: &TableProperty,
) -> bool {
    let left_ty = normalizer.arena.follow(left.ty);
    let right_ty = normalizer.arena.follow(right.ty);
    (left_ty == right_ty || normalizer.arena.get(left_ty) == normalizer.arena.get(right_ty))
        && left.read_only == right.read_only
        && left.write_only == right.write_only
        && left.deprecated == right.deprecated
}

fn remove_duplicate_type_shapes(arena: &Arena, types: &mut Vec<TypeId>) {
    let mut seen = Vec::new();
    types.retain(|ty| {
        let kind = arena.get(*ty);
        if seen.iter().any(|prior| prior == kind) {
            false
        } else {
            seen.push(kind.clone());
            true
        }
    });
}

fn collapse_singletons_covered_by_primitives(arena: &Arena, types: &mut Vec<TypeId>) {
    let primitives = primitive_options(arena, types);
    types.retain(|ty| match arena.get(*ty) {
        TypeKind::Singleton(singleton) => !primitives.contains(&singleton.primitive()),
        _ => true,
    });
}

fn collapse_boolean_singletons(arena: &Arena, types: &mut Vec<TypeId>) {
    let has_true = types
        .iter()
        .any(|ty| arena.get(*ty) == &TypeKind::Singleton(SingletonType::Boolean(true)));
    let has_false = types
        .iter()
        .any(|ty| arena.get(*ty) == &TypeKind::Singleton(SingletonType::Boolean(false)));
    if has_true && has_false {
        types.retain(|ty| {
            !matches!(
                arena.get(*ty),
                TypeKind::Singleton(SingletonType::Boolean(_))
            )
        });
        types.push(arena.primitives().boolean);
        remove_duplicate_types(types);
    }
}

fn collapse_covered_negated_singletons(arena: &mut Arena, types: &mut Vec<TypeId>) {
    let singletons: BTreeSet<_> = types
        .iter()
        .filter_map(|ty| match arena.get(*ty) {
            TypeKind::Singleton(_) => Some(*ty),
            _ => None,
        })
        .collect();
    if singletons.is_empty() {
        collapse_cofinite_intersections(arena, types);
        return;
    }

    let mut additions = Vec::new();
    types.retain(|ty| {
        let TypeKind::Intersection(options) = arena.get(*ty) else {
            return true;
        };
        let Some(primitive) = intersection_primitive(arena, options) else {
            return true;
        };
        let negated = negated_singleton_targets(arena, options, primitive);
        if negated.is_empty() || !negated.is_subset(&singletons) {
            return true;
        }
        additions.push(primitive_type(arena, primitive));
        false
    });
    types.extend(additions);
    remove_duplicate_types(types);
    collapse_cofinite_intersections(arena, types);
}

fn collapse_cofinite_intersections(arena: &mut Arena, types: &mut Vec<TypeId>) {
    let primitives = primitive_options(arena, types);
    if !primitives.is_empty() {
        types.retain(|ty| {
            !primitives
                .iter()
                .any(|primitive| cofinite_negated_singletons(arena, *ty, *primitive).is_some())
        });
        remove_duplicate_types(types);
    }

    for primitive in [
        PrimitiveType::Nil,
        PrimitiveType::Boolean,
        PrimitiveType::Number,
        PrimitiveType::String,
        PrimitiveType::Thread,
        PrimitiveType::Buffer,
        PrimitiveType::Vector,
    ] {
        let mut common_negated: Option<BTreeSet<TypeId>> = None;
        let mut cofinite_count = 0;
        for ty in types.iter() {
            let Some(negated) = cofinite_negated_singletons(arena, *ty, primitive) else {
                continue;
            };
            cofinite_count += 1;
            common_negated = Some(match common_negated {
                Some(common) => common.intersection(&negated).cloned().collect(),
                None => negated,
            });
        }
        if cofinite_count < 2 {
            continue;
        }
        let common_negated = common_negated.unwrap_or_default();
        let mut replacement = primitive_type(arena, primitive);
        if !common_negated.is_empty() {
            let mut members = vec![replacement];
            for singleton in common_negated {
                members.push(arena.alloc(TypeKind::Negation(singleton)));
            }
            replacement = arena.alloc(TypeKind::Intersection(members));
        }
        types.retain(|ty| cofinite_negated_singletons(arena, *ty, primitive).is_none());
        types.push(replacement);
        remove_duplicate_types(types);
        break;
    }
}

fn cofinite_negated_singletons(
    arena: &Arena,
    ty: TypeId,
    primitive: PrimitiveType,
) -> Option<BTreeSet<TypeId>> {
    let TypeKind::Intersection(options) = arena.get(ty) else {
        return None;
    };
    if intersection_primitive(arena, options) != Some(primitive) {
        return None;
    }
    let negated = negated_singleton_targets(arena, options, primitive);
    (!negated.is_empty()).then_some(negated)
}

fn negated_singleton_targets(
    arena: &Arena,
    options: &[TypeId],
    primitive: PrimitiveType,
) -> BTreeSet<TypeId> {
    options
        .iter()
        .filter_map(|option| match arena.get(*option) {
            TypeKind::Negation(target) => match arena.get(*target) {
                TypeKind::Singleton(singleton) if singleton.primitive() == primitive => {
                    Some(*target)
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn narrow_singletons_by_primitives(arena: &Arena, types: &mut Vec<TypeId>) {
    let primitives = primitive_options(arena, types);
    if primitives.is_empty() {
        return;
    }
    let matching_singletons: BTreeSet<_> = types
        .iter()
        .filter_map(|ty| match arena.get(*ty) {
            TypeKind::Singleton(singleton) if primitives.contains(&singleton.primitive()) => {
                Some(singleton.primitive())
            }
            _ => None,
        })
        .collect();
    types.retain(|ty| match arena.get(*ty) {
        TypeKind::Primitive(primitive) => !matching_singletons.contains(primitive),
        _ => true,
    });
}

fn order_intersection_options(arena: &Arena, types: &mut [TypeId]) {
    types.sort_by_key(|ty| match arena.get(*ty) {
        TypeKind::Free(_) | TypeKind::Generic(_) | TypeKind::Blocked(_) => (0, *ty),
        TypeKind::Negation(_) => (2, *ty),
        _ => (1, *ty),
    });
}

fn is_identifier_property_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn is_top_table_type(table: &TableType) -> bool {
    table.name.as_deref() == Some("table")
        && table.properties.is_empty()
        && table.indexer.is_none()
        && table.instantiated_type_params.is_empty()
}

fn alloc_top_table_type(arena: &mut Arena) -> TypeId {
    let mut table = TableType::new(TableState::Sealed);
    table.name = Some("table".to_owned());
    arena.alloc(TypeKind::Table(table))
}

fn collapse_functions_covered_by_top_function(arena: &Arena, types: &mut Vec<TypeId>) {
    let has_top_function = types.iter().any(|ty| {
        matches!(
            arena.get(*ty),
            TypeKind::Function(function) if is_top_function_type(arena, function)
        )
    });
    let has_specific_function = types.iter().any(|ty| {
        matches!(
            arena.get(*ty),
            TypeKind::Function(function) if !is_top_function_type(arena, function)
        )
    });
    if !has_top_function || !has_specific_function {
        return;
    }

    let mut kept_top_function = false;
    types.retain(|ty| match arena.get(*ty) {
        TypeKind::Function(function) if is_top_function_type(arena, function) => {
            if kept_top_function {
                false
            } else {
                kept_top_function = true;
                true
            }
        }
        TypeKind::Function(_) => false,
        _ => true,
    });
}

fn collapse_tables_covered_by_top_table(arena: &Arena, types: &mut Vec<TypeId>) {
    let has_top_table = types.iter().any(|ty| {
        matches!(
            arena.get(*ty),
            TypeKind::Table(table) if is_top_table_type(table)
        )
    });
    let has_specific_table = types.iter().any(|ty| {
        matches!(
            arena.get(*ty),
            TypeKind::Table(table) if !is_top_table_type(table)
        ) || matches!(arena.get(*ty), TypeKind::Metatable { .. })
    });
    if !has_top_table || !has_specific_table {
        return;
    }

    let mut kept_top_table = false;
    types.retain(|ty| match arena.get(*ty) {
        TypeKind::Table(table) if is_top_table_type(table) => {
            if kept_top_table {
                false
            } else {
                kept_top_table = true;
                true
            }
        }
        TypeKind::Table(_) | TypeKind::Metatable { .. } => false,
        _ => true,
    });
}

fn collapse_externs_covered_by_parents(arena: &Arena, types: &mut Vec<TypeId>) {
    let externs = types
        .iter()
        .filter_map(|ty| match arena.get(*ty) {
            TypeKind::Extern { name, parents, .. } => Some((*ty, name.clone(), parents.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    if externs.len() < 2 {
        return;
    }

    types.retain(|ty| {
        let TypeKind::Extern { name, parents, .. } = arena.get(*ty) else {
            return true;
        };
        !externs.iter().any(|(other_id, other_name, _)| {
            *other_id != *ty && extern_is_subtype(name, parents, other_name)
        })
    });
}

fn simplify_extern_intersections(arena: &Arena, types: &mut Vec<TypeId>) -> bool {
    loop {
        let mut changed = false;
        for left_index in 0..types.len() {
            let TypeKind::Extern {
                name: left_name,
                parents: left_parents,
                ..
            } = arena.get(types[left_index])
            else {
                continue;
            };
            for right_index in (left_index + 1)..types.len() {
                let TypeKind::Extern {
                    name: right_name,
                    parents: right_parents,
                    ..
                } = arena.get(types[right_index])
                else {
                    continue;
                };
                if extern_is_subtype(left_name, left_parents, right_name) {
                    types.remove(right_index);
                    changed = true;
                    break;
                }
                if extern_is_subtype(right_name, right_parents, left_name) {
                    types.remove(left_index);
                    changed = true;
                    break;
                }
                return false;
            }
            if changed {
                break;
            }
        }
        if !changed {
            return true;
        }
    }
}

fn collapse_negated_extern_intersections(arena: &Arena, negations: &mut Vec<(TypeId, TypeId)>) {
    let mut index = 0;
    while index < negations.len() {
        let mut removed = false;
        let (_, target) = negations[index];
        let TypeKind::Extern {
            name: target_name,
            parents: target_parents,
            ..
        } = arena.get(target)
        else {
            index += 1;
            continue;
        };
        for other_index in 0..negations.len() {
            if index == other_index {
                continue;
            }
            let (_, other_target) = negations[other_index];
            let TypeKind::Extern {
                name: other_name,
                parents: other_parents,
                ..
            } = arena.get(other_target)
            else {
                continue;
            };
            if extern_is_subtype(target_name, target_parents, other_name) {
                negations.remove(index);
                removed = true;
                break;
            }
            if extern_is_subtype(other_name, other_parents, target_name) {
                negations.remove(other_index);
                if other_index < index {
                    index -= 1;
                }
                removed = true;
                break;
            }
        }
        if !removed {
            index += 1;
        }
    }
}

fn intersection_primitive(arena: &Arena, types: &[TypeId]) -> Option<PrimitiveType> {
    let primitives = primitive_options(arena, types);
    match primitives.len() {
        1 => primitives.into_iter().next(),
        _ => None,
    }
}

fn incompatible_primitive_intersection(arena: &Arena, types: &[TypeId]) -> bool {
    primitive_options(arena, types).len() > 1
}

fn incompatible_singleton_intersection(arena: &Arena, types: &[TypeId]) -> bool {
    let mut seen = Vec::<SingletonType>::new();
    for ty in types {
        let TypeKind::Singleton(singleton) = arena.get(*ty) else {
            continue;
        };
        if seen
            .iter()
            .any(|prior| prior.primitive() == singleton.primitive() && prior != singleton)
        {
            return true;
        }
        seen.push(singleton.clone());
    }
    false
}

fn incompatible_primitive_singleton_intersection(arena: &Arena, types: &[TypeId]) -> bool {
    let primitives = primitive_options(arena, types);
    types.iter().any(|ty| match arena.get(*ty) {
        TypeKind::Singleton(singleton) => {
            !primitives.is_empty() && !primitives.contains(&singleton.primitive())
        }
        _ => false,
    })
}

fn incompatible_composite_scalar_intersection(arena: &Arena, types: &[TypeId]) -> bool {
    let has_composite = types.iter().any(|ty| {
        matches!(
            arena.get(*ty),
            TypeKind::Function(_)
                | TypeKind::Table(_)
                | TypeKind::Metatable { .. }
                | TypeKind::Extern { .. }
        )
    });
    has_composite
        && types.iter().any(|ty| {
            matches!(
                arena.get(*ty),
                TypeKind::Primitive(_) | TypeKind::Singleton(_)
            )
        })
}

fn incompatible_extern_runtime_intersection(arena: &Arena, types: &[TypeId]) -> bool {
    let has_extern = types
        .iter()
        .any(|ty| matches!(arena.get(*ty), TypeKind::Extern { .. }));
    has_extern
        && types.iter().any(|ty| {
            matches!(
                arena.get(*ty),
                TypeKind::Function(_) | TypeKind::Table(_) | TypeKind::Metatable { .. }
            )
        })
}

fn incompatible_function_table_intersection(arena: &Arena, types: &[TypeId]) -> bool {
    let has_function = types
        .iter()
        .any(|ty| matches!(arena.get(*ty), TypeKind::Function(_)));
    let has_table_like = types.iter().any(|ty| {
        matches!(
            arena.get(*ty),
            TypeKind::Table(_) | TypeKind::Metatable { .. }
        )
    });
    has_function && has_table_like
}

fn primitive_options(arena: &Arena, types: &[TypeId]) -> BTreeSet<PrimitiveType> {
    types
        .iter()
        .filter_map(|ty| match arena.get(*ty) {
            TypeKind::Primitive(primitive) => Some(*primitive),
            _ => None,
        })
        .collect()
}

fn primitive_type(arena: &Arena, primitive: PrimitiveType) -> TypeId {
    let primitives = arena.primitives();
    match primitive {
        PrimitiveType::Nil => primitives.nil,
        PrimitiveType::Boolean => primitives.boolean,
        PrimitiveType::Number => primitives.number,
        PrimitiveType::String => primitives.string,
        PrimitiveType::Thread => primitives.thread,
        PrimitiveType::Buffer => primitives.buffer,
        PrimitiveType::Vector => primitives.vector,
    }
}

fn is_indeterminate(arena: &Arena, ty: TypeId) -> bool {
    matches!(
        arena.get(ty),
        TypeKind::Free(_) | TypeKind::Generic(_) | TypeKind::Blocked(_)
    )
}

fn negated_disjoint_primitives_cover_unknown(arena: &Arena, types: &[TypeId]) -> bool {
    let mut negated = BTreeSet::new();
    for ty in types {
        let TypeKind::Negation(target) = arena.get(*ty) else {
            continue;
        };
        let TypeKind::Primitive(primitive) = arena.get(*target) else {
            continue;
        };
        negated.insert(*primitive);
    }
    negated.len() >= 2
}

fn extern_complement_root<'a>(name: &'a str, parents: &'a [String]) -> Option<&'a str> {
    if name == "userdata" {
        return Some(name);
    }
    parents
        .iter()
        .find_map(|parent| (parent == "userdata").then_some(parent.as_str()))
}

/// Simplifies a type in the supplied arena.
pub fn simplify_type(arena: &mut Arena, id: TypeId) -> TypeId {
    Normalizer::new(arena).simplify_type(id)
}

/// Test helper: normalize with extern/userdata negation expansion enabled.
///
/// Facade for the `normalize_tests.rs` cases that build arenas directly
/// (dense `BlockedType` construction, recursive table replacement, the
/// extern-negation ladder); source-driven tests use
/// `crate::test_context::TestContext::normalize_type` instead.
#[cfg(any())]
pub fn normalize_type(arena: &mut Arena, id: TypeId) -> TypeId {
    Normalizer::new(arena)
        .with_extern_negation_expansion()
        .simplify_type(id)
}

#[cfg(any())]
mod tests;
