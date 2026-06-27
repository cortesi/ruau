use std::collections::{BTreeMap, BTreeSet};

use crate::types::{
    Arena, FunctionType, GenericType, GenericTypePack, TypeId, TypeKind, TypePackId, TypePackKind,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct GenericInstantiationFrame {
    types: BTreeMap<TypeId, Option<TypeId>>,
    packs: BTreeMap<TypePackId, Option<TypePackId>>,
    rigid_types: BTreeSet<TypeId>,
    rigid_packs: BTreeSet<TypePackId>,
}

impl GenericInstantiationFrame {
    pub(super) fn for_function(arena: &Arena, function: &FunctionType) -> Self {
        let mut frame = Self::default();
        let mut seen_types = BTreeSet::new();
        let mut seen_packs = BTreeSet::new();
        frame.collect_pack_ids(
            arena,
            function.arguments,
            &function.generics,
            &function.generic_packs,
            &mut seen_types,
            &mut seen_packs,
        );
        frame.collect_pack_ids(
            arena,
            function.returns,
            &function.generics,
            &function.generic_packs,
            &mut seen_types,
            &mut seen_packs,
        );
        frame
    }

    pub(super) fn for_function_returns(arena: &Arena, function: &FunctionType) -> Self {
        let mut frame = Self::default();
        let mut seen_types = BTreeSet::new();
        let mut seen_packs = BTreeSet::new();
        frame.collect_pack_ids(
            arena,
            function.returns,
            &function.generics,
            &function.generic_packs,
            &mut seen_types,
            &mut seen_packs,
        );
        frame
    }

    pub(super) fn for_function_with_matching_generics(
        arena: &Arena,
        sub: &FunctionType,
        sup: &FunctionType,
    ) -> Self {
        let mut frame = Self::for_function(arena, sub);
        for (sub_generic, sup_generic) in sub.generics.iter().zip(&sup.generics) {
            let Some(sub_id) = generic_type_id_in_function(arena, sub, sub_generic) else {
                continue;
            };
            let Some(sup_id) = generic_type_id_in_function(arena, sup, sup_generic) else {
                continue;
            };
            frame.rigid_types.insert(sup_id);
            if frame.contains_type(sub_id) {
                frame.bind_type(sub_id, sup_id);
            }
        }
        for (sub_pack, sup_pack) in sub.generic_packs.iter().zip(&sup.generic_packs) {
            let Some(sub_id) = generic_pack_id_in_function(arena, sub, sub_pack) else {
                continue;
            };
            let Some(sup_id) = generic_pack_id_in_function(arena, sup, sup_pack) else {
                continue;
            };
            frame.rigid_packs.insert(sup_id);
            if frame.contains_pack(sub_id) {
                frame.bind_pack(sub_id, sup_id);
            }
        }
        frame
    }

    pub(super) fn for_function_with_rigid_super_generics(
        arena: &Arena,
        sub: &FunctionType,
        sup: &FunctionType,
    ) -> Self {
        let mut frame = Self::for_function(arena, sub);
        for generic in &sup.generics {
            if let Some(id) = generic_type_id_in_function(arena, sup, generic) {
                frame.rigid_types.insert(id);
            }
        }
        for generic_pack in &sup.generic_packs {
            if let Some(id) = generic_pack_id_in_function(arena, sup, generic_pack) {
                frame.rigid_packs.insert(id);
            }
        }
        frame
    }

    pub(super) fn is_empty(&self) -> bool {
        self.types.is_empty() && self.packs.is_empty()
    }

    pub(super) fn contains_type(&self, id: TypeId) -> bool {
        self.types.contains_key(&id)
    }

    pub(super) fn contains_pack(&self, id: TypePackId) -> bool {
        self.packs.contains_key(&id)
    }

    pub(super) fn is_rigid_type(&self, id: TypeId) -> bool {
        self.rigid_types.contains(&id)
    }

    pub(super) fn is_rigid_pack(&self, id: TypePackId) -> bool {
        self.rigid_packs.contains(&id)
    }

    pub(super) fn type_binding(&self, id: TypeId) -> Option<TypeId> {
        self.types.get(&id).copied().flatten()
    }

    pub(super) fn pack_binding(&self, id: TypePackId) -> Option<TypePackId> {
        self.packs.get(&id).copied().flatten()
    }

    pub(super) fn bind_type(&mut self, generic: TypeId, bound: TypeId) {
        self.types.insert(generic, Some(bound));
    }

    pub(super) fn bind_pack(&mut self, generic: TypePackId, bound: TypePackId) {
        self.packs.insert(generic, Some(bound));
    }

    fn collect_type_ids(
        &mut self,
        arena: &Arena,
        id: TypeId,
        generics: &[GenericType],
        generic_packs: &[GenericTypePack],
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) {
        let id = arena.follow(id);
        if !seen_types.insert(id) {
            return;
        }

        match arena.get(id) {
            TypeKind::Generic(generic) if generics.iter().any(|owned| owned == generic) => {
                self.types.entry(id).or_insert(None);
            }
            TypeKind::Function(function) => {
                let visible_generics: Vec<_> = generics
                    .iter()
                    .filter(|generic| !function.generics.contains(*generic))
                    .cloned()
                    .collect();
                let visible_generic_packs: Vec<_> = generic_packs
                    .iter()
                    .filter(|pack| !function.generic_packs.contains(*pack))
                    .cloned()
                    .collect();
                self.collect_pack_ids(
                    arena,
                    function.arguments,
                    &visible_generics,
                    &visible_generic_packs,
                    seen_types,
                    seen_packs,
                );
                self.collect_pack_ids(
                    arena,
                    function.returns,
                    &visible_generics,
                    &visible_generic_packs,
                    seen_types,
                    seen_packs,
                );
            }
            TypeKind::Table(table) => {
                for ty in table.instantiated_type_params.iter().copied() {
                    self.collect_type_ids(
                        arena,
                        ty,
                        generics,
                        generic_packs,
                        seen_types,
                        seen_packs,
                    );
                }
                for property in table.properties.values() {
                    self.collect_type_ids(
                        arena,
                        property.ty,
                        generics,
                        generic_packs,
                        seen_types,
                        seen_packs,
                    );
                }
                if let Some(indexer) = &table.indexer {
                    self.collect_type_ids(
                        arena,
                        indexer.key,
                        generics,
                        generic_packs,
                        seen_types,
                        seen_packs,
                    );
                    self.collect_type_ids(
                        arena,
                        indexer.value,
                        generics,
                        generic_packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Extern { properties, .. } => {
                for property in properties.values() {
                    self.collect_type_ids(
                        arena,
                        property.ty,
                        generics,
                        generic_packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.collect_type_ids(
                    arena,
                    *table,
                    generics,
                    generic_packs,
                    seen_types,
                    seen_packs,
                );
                self.collect_type_ids(
                    arena,
                    *metatable,
                    generics,
                    generic_packs,
                    seen_types,
                    seen_packs,
                );
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => {
                for ty in arguments.iter().copied() {
                    self.collect_type_ids(
                        arena,
                        ty,
                        generics,
                        generic_packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Negation(ty) | TypeKind::Bound(ty) => {
                self.collect_type_ids(arena, *ty, generics, generic_packs, seen_types, seen_packs);
            }
            TypeKind::Free(variable) => {
                for ty in [variable.lower_bound, variable.upper_bound]
                    .into_iter()
                    .flatten()
                {
                    self.collect_type_ids(
                        arena,
                        ty,
                        generics,
                        generic_packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => {}
        }
    }

    fn collect_pack_ids(
        &mut self,
        arena: &Arena,
        id: TypePackId,
        generics: &[GenericType],
        generic_packs: &[GenericTypePack],
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) {
        let id = arena.follow_pack(id);
        if !seen_packs.insert(id) {
            return;
        }

        match arena.get_pack(id) {
            TypePackKind::Generic(pack) if generic_packs.iter().any(|owned| owned == pack) => {
                self.packs.entry(id).or_insert(None);
            }
            TypePackKind::List { types, tail } => {
                for ty in types.iter().copied() {
                    self.collect_type_ids(
                        arena,
                        ty,
                        generics,
                        generic_packs,
                        seen_types,
                        seen_packs,
                    );
                }
                if let Some(tail) = tail {
                    self.collect_pack_ids(
                        arena,
                        *tail,
                        generics,
                        generic_packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypePackKind::Variadic { ty } => {
                self.collect_type_ids(arena, *ty, generics, generic_packs, seen_types, seen_packs);
            }
            TypePackKind::Bound(bound) => {
                self.collect_pack_ids(
                    arena,
                    *bound,
                    generics,
                    generic_packs,
                    seen_types,
                    seen_packs,
                );
            }
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => {}
        }
    }
}

fn generic_type_id_in_function(
    arena: &Arena,
    function: &FunctionType,
    generic: &GenericType,
) -> Option<TypeId> {
    let mut seen_types = BTreeSet::new();
    let mut seen_packs = BTreeSet::new();
    generic_type_id_in_function_with_seen(
        arena,
        function,
        generic,
        &mut seen_types,
        &mut seen_packs,
    )
}

fn generic_type_id_in_function_with_seen(
    arena: &Arena,
    function: &FunctionType,
    generic: &GenericType,
    seen_types: &mut BTreeSet<TypeId>,
    seen_packs: &mut BTreeSet<TypePackId>,
) -> Option<TypeId> {
    generic_type_id_in_pack(arena, function.arguments, generic, seen_types, seen_packs).or_else(
        || generic_type_id_in_pack(arena, function.returns, generic, seen_types, seen_packs),
    )
}

fn generic_type_id_in_pack(
    arena: &Arena,
    id: TypePackId,
    generic: &GenericType,
    seen_types: &mut BTreeSet<TypeId>,
    seen_packs: &mut BTreeSet<TypePackId>,
) -> Option<TypeId> {
    let id = arena.follow_pack(id);
    if !seen_packs.insert(id) {
        return None;
    }
    match arena.get_pack(id) {
        TypePackKind::List { types, tail } => types
            .iter()
            .find_map(|ty| generic_type_id_in_type(arena, *ty, generic, seen_types, seen_packs))
            .or_else(|| {
                tail.and_then(|tail| {
                    generic_type_id_in_pack(arena, tail, generic, seen_types, seen_packs)
                })
            }),
        TypePackKind::Variadic { ty } => {
            generic_type_id_in_type(arena, *ty, generic, seen_types, seen_packs)
        }
        TypePackKind::Bound(bound) => {
            generic_type_id_in_pack(arena, *bound, generic, seen_types, seen_packs)
        }
        TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => None,
    }
}

fn generic_type_id_in_type(
    arena: &Arena,
    id: TypeId,
    generic: &GenericType,
    seen_types: &mut BTreeSet<TypeId>,
    seen_packs: &mut BTreeSet<TypePackId>,
) -> Option<TypeId> {
    let id = arena.follow(id);
    if !seen_types.insert(id) {
        return None;
    }
    match arena.get(id) {
        TypeKind::Generic(found) if found == generic => Some(id),
        TypeKind::Function(function) if !function.generics.contains(generic) => {
            generic_type_id_in_function_with_seen(arena, function, generic, seen_types, seen_packs)
        }
        TypeKind::Table(table) => table
            .instantiated_type_params
            .iter()
            .chain(table.properties.values().map(|property| &property.ty))
            .find_map(|ty| generic_type_id_in_type(arena, *ty, generic, seen_types, seen_packs))
            .or_else(|| {
                table.indexer.as_ref().and_then(|indexer| {
                    generic_type_id_in_type(arena, indexer.key, generic, seen_types, seen_packs)
                        .or_else(|| {
                            generic_type_id_in_type(
                                arena,
                                indexer.value,
                                generic,
                                seen_types,
                                seen_packs,
                            )
                        })
                })
            }),
        TypeKind::Extern { properties, .. } => properties.values().find_map(|property| {
            generic_type_id_in_type(arena, property.ty, generic, seen_types, seen_packs)
        }),
        TypeKind::Metatable {
            table, metatable, ..
        } => {
            generic_type_id_in_type(arena, *table, generic, seen_types, seen_packs).or_else(|| {
                generic_type_id_in_type(arena, *metatable, generic, seen_types, seen_packs)
            })
        }
        TypeKind::TypeFunctionInstance { arguments, .. }
        | TypeKind::Union(arguments)
        | TypeKind::Intersection(arguments) => arguments
            .iter()
            .find_map(|ty| generic_type_id_in_type(arena, *ty, generic, seen_types, seen_packs)),
        TypeKind::Negation(ty) | TypeKind::Bound(ty) => {
            generic_type_id_in_type(arena, *ty, generic, seen_types, seen_packs)
        }
        TypeKind::Free(variable) => [variable.lower_bound, variable.upper_bound]
            .into_iter()
            .flatten()
            .find_map(|ty| generic_type_id_in_type(arena, ty, generic, seen_types, seen_packs)),
        TypeKind::Primitive(_)
        | TypeKind::Singleton(_)
        | TypeKind::Generic(_)
        | TypeKind::Blocked(_)
        | TypeKind::Error
        | TypeKind::Unknown
        | TypeKind::Never
        | TypeKind::Any
        | TypeKind::Function(_) => None,
    }
}

fn generic_pack_id_in_function(
    arena: &Arena,
    function: &FunctionType,
    generic: &GenericTypePack,
) -> Option<TypePackId> {
    let mut seen_types = BTreeSet::new();
    let mut seen_packs = BTreeSet::new();
    generic_pack_id_in_function_with_seen(
        arena,
        function,
        generic,
        &mut seen_types,
        &mut seen_packs,
    )
}

fn generic_pack_id_in_function_with_seen(
    arena: &Arena,
    function: &FunctionType,
    generic: &GenericTypePack,
    seen_types: &mut BTreeSet<TypeId>,
    seen_packs: &mut BTreeSet<TypePackId>,
) -> Option<TypePackId> {
    generic_pack_id_in_pack(arena, function.arguments, generic, seen_types, seen_packs).or_else(
        || generic_pack_id_in_pack(arena, function.returns, generic, seen_types, seen_packs),
    )
}

fn generic_pack_id_in_pack(
    arena: &Arena,
    id: TypePackId,
    generic: &GenericTypePack,
    seen_types: &mut BTreeSet<TypeId>,
    seen_packs: &mut BTreeSet<TypePackId>,
) -> Option<TypePackId> {
    let id = arena.follow_pack(id);
    if !seen_packs.insert(id) {
        return None;
    }
    match arena.get_pack(id) {
        TypePackKind::Generic(found) if found == generic => Some(id),
        TypePackKind::List { types, tail } => types
            .iter()
            .find_map(|ty| generic_pack_id_in_type(arena, *ty, generic, seen_types, seen_packs))
            .or_else(|| {
                tail.and_then(|tail| {
                    generic_pack_id_in_pack(arena, tail, generic, seen_types, seen_packs)
                })
            }),
        TypePackKind::Variadic { ty } => {
            generic_pack_id_in_type(arena, *ty, generic, seen_types, seen_packs)
        }
        TypePackKind::Bound(bound) => {
            generic_pack_id_in_pack(arena, *bound, generic, seen_types, seen_packs)
        }
        TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => None,
    }
}

fn generic_pack_id_in_type(
    arena: &Arena,
    id: TypeId,
    generic: &GenericTypePack,
    seen_types: &mut BTreeSet<TypeId>,
    seen_packs: &mut BTreeSet<TypePackId>,
) -> Option<TypePackId> {
    let id = arena.follow(id);
    if !seen_types.insert(id) {
        return None;
    }
    match arena.get(id) {
        TypeKind::Function(function) if !function.generic_packs.contains(generic) => {
            generic_pack_id_in_function_with_seen(arena, function, generic, seen_types, seen_packs)
        }
        TypeKind::Table(table) => table
            .instantiated_type_params
            .iter()
            .chain(table.properties.values().map(|property| &property.ty))
            .find_map(|ty| generic_pack_id_in_type(arena, *ty, generic, seen_types, seen_packs))
            .or_else(|| {
                table.indexer.as_ref().and_then(|indexer| {
                    generic_pack_id_in_type(arena, indexer.key, generic, seen_types, seen_packs)
                        .or_else(|| {
                            generic_pack_id_in_type(
                                arena,
                                indexer.value,
                                generic,
                                seen_types,
                                seen_packs,
                            )
                        })
                })
            }),
        TypeKind::Extern { properties, .. } => properties.values().find_map(|property| {
            generic_pack_id_in_type(arena, property.ty, generic, seen_types, seen_packs)
        }),
        TypeKind::Metatable {
            table, metatable, ..
        } => {
            generic_pack_id_in_type(arena, *table, generic, seen_types, seen_packs).or_else(|| {
                generic_pack_id_in_type(arena, *metatable, generic, seen_types, seen_packs)
            })
        }
        TypeKind::TypeFunctionInstance { arguments, .. }
        | TypeKind::Union(arguments)
        | TypeKind::Intersection(arguments) => arguments
            .iter()
            .find_map(|ty| generic_pack_id_in_type(arena, *ty, generic, seen_types, seen_packs)),
        TypeKind::Negation(ty) | TypeKind::Bound(ty) => {
            generic_pack_id_in_type(arena, *ty, generic, seen_types, seen_packs)
        }
        TypeKind::Free(variable) => [variable.lower_bound, variable.upper_bound]
            .into_iter()
            .flatten()
            .find_map(|ty| generic_pack_id_in_type(arena, ty, generic, seen_types, seen_packs)),
        TypeKind::Primitive(_)
        | TypeKind::Singleton(_)
        | TypeKind::Function(_)
        | TypeKind::Generic(_)
        | TypeKind::Blocked(_)
        | TypeKind::Error
        | TypeKind::Unknown
        | TypeKind::Never
        | TypeKind::Any => None,
    }
}
