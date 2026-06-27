//! Reusable type-graph rewriting passes.

use std::collections::BTreeMap;

use crate::types::{
    Arena, FunctionType, TableState, TableType, TypeId, TypeKind, TypePackId, TypePackKind,
};

/// Copies a type graph, replacing free types with `any` and free packs with an
/// `...any` pack while preserving recursive structural edges.
#[cfg(any())]
pub fn anyify_type_graph(arena: &mut Arena, id: TypeId) -> TypeId {
    TypeGraphFreeReplacer::new(arena, FreeReplacement::Any).rewrite_type(id)
}

/// Copies a type graph for a module public surface, replacing unresolved free
/// types and packs with errors while preserving recursive structural edges.
pub fn publicize_type_graph(arena: &mut Arena, id: TypeId) -> TypeId {
    TypeGraphFreeReplacer::new(arena, FreeReplacement::Error).rewrite_type(id)
}

/// Replacement strategy for free types and packs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FreeReplacement {
    /// Replace free types with `any` and free packs with `...any`.
    #[allow(dead_code)]
    Any,
    /// Replace free types with `*error-type*` and free packs with `...*error-type*`.
    Error,
}

/// Arena graph copier used by free-variable replacement passes.
struct TypeGraphFreeReplacer<'arena> {
    /// Arena being copied into.
    arena: &'arena mut Arena,
    /// Old type handles to their copied handles.
    types: BTreeMap<TypeId, TypeId>,
    /// Old pack handles to their copied handles.
    packs: BTreeMap<TypePackId, TypePackId>,
    /// Replacement strategy.
    replacement: FreeReplacement,
    /// Lazily allocated replacement for free packs.
    free_pack: Option<TypePackId>,
}

impl<'arena> TypeGraphFreeReplacer<'arena> {
    /// Creates a replacer for one substitution run.
    fn new(arena: &'arena mut Arena, replacement: FreeReplacement) -> Self {
        Self {
            arena,
            types: BTreeMap::new(),
            packs: BTreeMap::new(),
            replacement,
            free_pack: None,
        }
    }

    /// Copies and rewrites a type graph.
    fn rewrite_type(&mut self, id: TypeId) -> TypeId {
        let id = self.arena.follow(id);
        if let Some(mapped) = self.types.get(&id) {
            return *mapped;
        }

        match self.arena.get(id).clone() {
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => id,
            TypeKind::Free(_) => self.free_type(),
            TypeKind::Blocked(_) => id,
            TypeKind::Bound(bound) => self.rewrite_type(bound),
            TypeKind::Function(mut function) => {
                let copy = self.arena.alloc(TypeKind::Function(FunctionType::new(
                    self.arena.empty_pack(),
                    self.arena.empty_pack(),
                )));
                self.types.insert(id, copy);
                function.arguments = self.rewrite_pack(function.arguments);
                function.returns = self.rewrite_pack(function.returns);
                self.arena.replace(copy, TypeKind::Function(function));
                copy
            }
            TypeKind::Table(mut table) => {
                let copy = self
                    .arena
                    .alloc(TypeKind::Table(TableType::new(table.state)));
                self.types.insert(id, copy);
                if matches!(table.state, TableState::Free | TableState::Unsealed) {
                    table.seal();
                }
                for property in table.properties.values_mut() {
                    property.ty = self.rewrite_type(property.ty);
                }
                if let Some(indexer) = table.indexer.as_mut() {
                    indexer.key = self.rewrite_type(indexer.key);
                    indexer.value = self.rewrite_type(indexer.value);
                }
                for parameter in &mut table.instantiated_type_params {
                    *parameter = self.rewrite_type(*parameter);
                }
                self.arena.replace(copy, TypeKind::Table(table));
                copy
            }
            TypeKind::Metatable {
                table,
                metatable,
                name,
            } => {
                let copy = self.arena.alloc(TypeKind::Metatable {
                    table,
                    metatable,
                    name: name.clone(),
                });
                self.types.insert(id, copy);
                let cloned_table = self.rewrite_type(table);
                let cloned_metatable = self.rewrite_type(metatable);
                self.arena.replace(
                    copy,
                    TypeKind::Metatable {
                        table: cloned_table,
                        metatable: cloned_metatable,
                        name,
                    },
                );
                copy
            }
            TypeKind::TypeFunctionInstance { name, arguments } => {
                let copy = self.arena.alloc(TypeKind::TypeFunctionInstance {
                    name: name.clone(),
                    arguments: Vec::new(),
                });
                self.types.insert(id, copy);
                let arguments = arguments
                    .into_iter()
                    .map(|argument| self.rewrite_type(argument))
                    .collect();
                self.arena
                    .replace(copy, TypeKind::TypeFunctionInstance { name, arguments });
                copy
            }
            TypeKind::Union(options) => {
                let copy = self.arena.alloc(TypeKind::Union(Vec::new()));
                self.types.insert(id, copy);
                let options = options
                    .into_iter()
                    .map(|option| self.rewrite_type(option))
                    .collect();
                self.arena.replace(copy, TypeKind::Union(options));
                copy
            }
            TypeKind::Intersection(parts) => {
                let copy = self.arena.alloc(TypeKind::Intersection(Vec::new()));
                self.types.insert(id, copy);
                let parts = parts
                    .into_iter()
                    .map(|part| self.rewrite_type(part))
                    .collect();
                self.arena.replace(copy, TypeKind::Intersection(parts));
                copy
            }
            TypeKind::Negation(ty) => {
                let copy = self.arena.alloc(TypeKind::Negation(ty));
                self.types.insert(id, copy);
                let ty = self.rewrite_type(ty);
                self.arena.replace(copy, TypeKind::Negation(ty));
                copy
            }
        }
    }

    /// Copies and rewrites a type-pack graph.
    fn rewrite_pack(&mut self, id: TypePackId) -> TypePackId {
        let id = self.arena.follow_pack(id);
        if let Some(mapped) = self.packs.get(&id) {
            return *mapped;
        }

        match self.arena.get_pack(id).clone() {
            TypePackKind::List { types, tail } => {
                let copy = self.arena.alloc_pack(TypePackKind::List {
                    types: Vec::new(),
                    tail: None,
                });
                self.packs.insert(id, copy);
                let types = types.into_iter().map(|ty| self.rewrite_type(ty)).collect();
                let tail = tail.map(|tail| self.rewrite_pack(tail));
                self.arena
                    .replace_pack(copy, TypePackKind::List { types, tail });
                copy
            }
            TypePackKind::Variadic { ty } => {
                let copy = self.arena.alloc_pack(TypePackKind::Variadic { ty });
                self.packs.insert(id, copy);
                let ty = self.rewrite_type(ty);
                self.arena.replace_pack(copy, TypePackKind::Variadic { ty });
                copy
            }
            TypePackKind::Free { .. } => self.free_pack(),
            TypePackKind::Generic(_) | TypePackKind::Error => id,
            TypePackKind::Bound(bound) => self.rewrite_pack(bound),
        }
    }

    /// Returns the replacement type for free types.
    fn free_type(&self) -> TypeId {
        match self.replacement {
            FreeReplacement::Any => self.arena.primitives().any,
            FreeReplacement::Error => self.arena.primitives().error,
        }
    }

    /// Returns the shared free-pack replacement.
    fn free_pack(&mut self) -> TypePackId {
        if let Some(pack) = self.free_pack {
            return pack;
        }
        let pack = match self.replacement {
            FreeReplacement::Any => self.arena.alloc_pack(TypePackKind::Variadic {
                ty: self.arena.primitives().any,
            }),
            FreeReplacement::Error => self.arena.alloc_pack(TypePackKind::Error),
        };
        self.free_pack = Some(pack);
        pack
    }
}
