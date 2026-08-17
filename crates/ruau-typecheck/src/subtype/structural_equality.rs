use super::Subtyper;
use crate::types::{
    TableIndexer, TableProperty, TypeId, TypeKind, TypePackId, TypePackKind,
    same_named_table_instance,
};

impl Subtyper<'_> {
    pub(super) fn structurally_equal_type(&mut self, left: TypeId, right: TypeId) -> bool {
        let left = self.arena.follow(left);
        let right = self.arena.follow(right);
        if left == right {
            return true;
        }
        let key = ordered_pair(left, right);
        if self.structurally_equal_types.contains(&key) {
            return true;
        }
        if let Some(&dependency) = self.structural_equality_types_in_progress.get(&key) {
            self.structural_equality_min_dependency =
                self.structural_equality_min_dependency.min(dependency);
            return true;
        }

        let entry_depth = self.structural_equality_clock;
        self.structural_equality_clock += 1;
        self.structural_equality_types_in_progress
            .insert(key, entry_depth);
        let saved_min_dependency = self.structural_equality_min_dependency;
        self.structural_equality_min_dependency = usize::MAX;
        let equal = self.structurally_equal_type_kinds(
            self.arena.get(left).clone(),
            self.arena.get(right).clone(),
        );
        let subtree_min_dependency = self.structural_equality_min_dependency;
        self.structural_equality_min_dependency = saved_min_dependency.min(subtree_min_dependency);
        self.structural_equality_types_in_progress.remove(&key);
        if equal && subtree_min_dependency >= entry_depth {
            self.structurally_equal_types.insert(key);
        }
        equal
    }

    fn structurally_equal_type_kinds(&mut self, left: TypeKind, right: TypeKind) -> bool {
        match (left, right) {
            (TypeKind::Primitive(left), TypeKind::Primitive(right)) => left == right,
            (TypeKind::Singleton(left), TypeKind::Singleton(right)) => left == right,
            (TypeKind::Function(left), TypeKind::Function(right)) => {
                left.generics == right.generics
                    && left.generic_packs == right.generic_packs
                    && left.argument_names == right.argument_names
                    && left.has_self == right.has_self
                    && left.is_checked == right.is_checked
                    && self.structurally_equal_pack(left.arguments, right.arguments)
                    && self.structurally_equal_pack(left.returns, right.returns)
            }
            (TypeKind::Table(left), TypeKind::Table(right)) => {
                same_named_table_instance(self.arena, &left, &right)
                    || (left.name == right.name
                        && left.alias_identity == right.alias_identity
                        && left.state == right.state
                        && self.structurally_equal_type_lists(
                            &left.instantiated_type_params,
                            &right.instantiated_type_params,
                        )
                        && self.structurally_equal_pack_lists(
                            &left.instantiated_type_pack_params,
                            &right.instantiated_type_pack_params,
                        )
                        && self.structurally_equal_properties(&left.properties, &right.properties)
                        && self.structurally_equal_indexers(
                            left.indexer.as_ref(),
                            right.indexer.as_ref(),
                        ))
            }
            (
                TypeKind::Extern {
                    name: left_name,
                    parents: left_parents,
                    properties: left_properties,
                    indexer: left_indexer,
                },
                TypeKind::Extern {
                    name: right_name,
                    parents: right_parents,
                    properties: right_properties,
                    indexer: right_indexer,
                },
            ) => {
                left_name == right_name
                    && left_parents == right_parents
                    && self.structurally_equal_properties(&left_properties, &right_properties)
                    && self
                        .structurally_equal_indexers(left_indexer.as_ref(), right_indexer.as_ref())
            }
            (
                TypeKind::Metatable {
                    table: left_table,
                    metatable: left_metatable,
                    name: left_name,
                },
                TypeKind::Metatable {
                    table: right_table,
                    metatable: right_metatable,
                    name: right_name,
                },
            ) => {
                left_name == right_name
                    && self.structurally_equal_type(left_table, right_table)
                    && self.structurally_equal_type(left_metatable, right_metatable)
            }
            (
                TypeKind::TypeFunctionInstance {
                    name: left_name,
                    arguments: left_arguments,
                },
                TypeKind::TypeFunctionInstance {
                    name: right_name,
                    arguments: right_arguments,
                },
            ) => {
                left_name == right_name
                    && self.structurally_equal_type_lists(&left_arguments, &right_arguments)
            }
            (TypeKind::Union(left), TypeKind::Union(right))
            | (TypeKind::Intersection(left), TypeKind::Intersection(right)) => {
                self.structurally_equal_type_lists(&left, &right)
            }
            (TypeKind::Negation(left), TypeKind::Negation(right)) => {
                self.structurally_equal_type(left, right)
            }
            (TypeKind::Error, TypeKind::Error)
            | (TypeKind::Unknown, TypeKind::Unknown)
            | (TypeKind::Never, TypeKind::Never)
            | (TypeKind::Any, TypeKind::Any) => true,
            (TypeKind::Bound(_), _) | (_, TypeKind::Bound(_)) => {
                unreachable!("follow removes bound types")
            }
            (
                TypeKind::Free(_) | TypeKind::Blocked(_) | TypeKind::Generic(_),
                TypeKind::Free(_) | TypeKind::Blocked(_) | TypeKind::Generic(_),
            ) => false,
            _ => false,
        }
    }

    fn structurally_equal_pack(&mut self, left: TypePackId, right: TypePackId) -> bool {
        let left = self.arena.follow_pack(left);
        let right = self.arena.follow_pack(right);
        if left == right {
            return true;
        }
        let key = ordered_pair(left, right);
        if self.structurally_equal_packs.contains(&key) {
            return true;
        }
        if let Some(&dependency) = self.structural_equality_packs_in_progress.get(&key) {
            self.structural_equality_min_dependency =
                self.structural_equality_min_dependency.min(dependency);
            return true;
        }

        let entry_depth = self.structural_equality_clock;
        self.structural_equality_clock += 1;
        self.structural_equality_packs_in_progress
            .insert(key, entry_depth);
        let saved_min_dependency = self.structural_equality_min_dependency;
        self.structural_equality_min_dependency = usize::MAX;
        let equal = match (
            self.arena.get_pack(left).clone(),
            self.arena.get_pack(right).clone(),
        ) {
            (
                TypePackKind::List {
                    types: left_types,
                    tail: left_tail,
                },
                TypePackKind::List {
                    types: right_types,
                    tail: right_tail,
                },
            ) => {
                self.structurally_equal_type_lists(&left_types, &right_types)
                    && match (left_tail, right_tail) {
                        (Some(left), Some(right)) => self.structurally_equal_pack(left, right),
                        (None, None) => true,
                        _ => false,
                    }
            }
            (TypePackKind::Variadic { ty: left }, TypePackKind::Variadic { ty: right }) => {
                self.structurally_equal_type(left, right)
            }
            (TypePackKind::Error, TypePackKind::Error) => true,
            (TypePackKind::Bound(_), _) | (_, TypePackKind::Bound(_)) => {
                unreachable!("follow_pack removes bound packs")
            }
            (
                TypePackKind::Free { .. } | TypePackKind::Generic(_),
                TypePackKind::Free { .. } | TypePackKind::Generic(_),
            ) => false,
            _ => false,
        };
        let subtree_min_dependency = self.structural_equality_min_dependency;
        self.structural_equality_min_dependency = saved_min_dependency.min(subtree_min_dependency);
        self.structural_equality_packs_in_progress.remove(&key);
        if equal && subtree_min_dependency >= entry_depth {
            self.structurally_equal_packs.insert(key);
        }
        equal
    }

    fn structurally_equal_type_lists(&mut self, left: &[TypeId], right: &[TypeId]) -> bool {
        left.len() == right.len()
            && left
                .iter()
                .copied()
                .zip(right.iter().copied())
                .all(|(left, right)| self.structurally_equal_type(left, right))
    }

    fn structurally_equal_pack_lists(&mut self, left: &[TypePackId], right: &[TypePackId]) -> bool {
        left.len() == right.len()
            && left
                .iter()
                .copied()
                .zip(right.iter().copied())
                .all(|(left, right)| self.structurally_equal_pack(left, right))
    }

    fn structurally_equal_properties(
        &mut self,
        left: &std::collections::BTreeMap<String, TableProperty>,
        right: &std::collections::BTreeMap<String, TableProperty>,
    ) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(
                |((left_name, left_property), (right_name, right_property))| {
                    left_name == right_name
                        && self.structurally_equal_property(left_property, right_property)
                },
            )
    }

    fn structurally_equal_property(&mut self, left: &TableProperty, right: &TableProperty) -> bool {
        left.location == right.location
            && left.documentation_symbol == right.documentation_symbol
            && left.read_only == right.read_only
            && left.write_only == right.write_only
            && left.deprecated == right.deprecated
            && self.structurally_equal_type(left.ty, right.ty)
            && match (left.write_ty, right.write_ty) {
                (Some(left), Some(right)) => self.structurally_equal_type(left, right),
                (None, None) => true,
                _ => false,
            }
    }

    fn structurally_equal_indexers(
        &mut self,
        left: Option<&TableIndexer>,
        right: Option<&TableIndexer>,
    ) -> bool {
        match (left, right) {
            (Some(left), Some(right)) => {
                left.read_only == right.read_only
                    && self.structurally_equal_type(left.key, right.key)
                    && self.structurally_equal_type(left.value, right.value)
            }
            (None, None) => true,
            _ => false,
        }
    }
}

fn ordered_pair<T: Copy + Ord>(left: T, right: T) -> (T, T) {
    if left <= right {
        (left, right)
    } else {
        (right, left)
    }
}
