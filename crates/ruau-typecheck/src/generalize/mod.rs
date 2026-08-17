//! Generalization and instantiation for arena-owned types.

use std::collections::{BTreeMap, BTreeSet};

use crate::types::{
    Arena, BlockedType, FunctionType, GenericType, GenericTypePack, TableIndexer, TableType,
    TypeId, TypeKind, TypeLevel, TypePackId, TypePackKind, TypeVariable,
};

/// Instantiates generic variables into fresh free variables at a target level.
pub struct Instantiator<'a> {
    arena: &'a mut Arena,
    level: TypeLevel,
    types: BTreeMap<(String, TypeLevel), TypeId>,
    packs: BTreeMap<(String, TypeLevel), TypePackId>,
    function_depth: usize,
    root_function: Option<TypeId>,
    /// Maps each original type to its instantiated copy. Filled with a
    /// placeholder before recursing so self-referential types resolve to the
    /// in-progress copy instead of recursing forever, and so shared subgraphs
    /// are instantiated once.
    type_cache: BTreeMap<TypeId, TypeId>,
    pack_cache: BTreeMap<TypePackId, TypePackId>,
}

impl<'a> Instantiator<'a> {
    /// Creates an instantiator that allocates fresh free variables at `level`.
    pub fn new(arena: &'a mut Arena, level: TypeLevel) -> Self {
        Self {
            arena,
            level,
            types: BTreeMap::new(),
            packs: BTreeMap::new(),
            function_depth: 0,
            root_function: None,
            type_cache: BTreeMap::new(),
            pack_cache: BTreeMap::new(),
        }
    }

    /// Instantiates a type graph.
    pub fn instantiate_type(&mut self, id: TypeId) -> TypeId {
        let id = self.arena.follow(id);
        if matches!(self.arena.get(id), TypeKind::Bound(_)) {
            return self.arena.primitives().error;
        }
        if self.function_depth > 0 && self.root_function == Some(id) {
            return id;
        }
        if let Some(cached) = self.type_cache.get(&id) {
            return *cached;
        }
        match self.arena.get(id).clone() {
            TypeKind::Generic(generic) => self.instantiate_generic(generic),
            TypeKind::Bound(_) => unreachable!("bound cycles return the error type above"),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => id,
            kind => {
                // Reserve a placeholder before recursing so a self-reference
                // resolves to this in-progress copy instead of looping.
                let placeholder = self.arena.alloc(TypeKind::Blocked(BlockedType::default()));
                self.type_cache.insert(id, placeholder);
                let previous_root = if matches!(kind, TypeKind::Function(_))
                    && self.function_depth == 0
                    && self.root_function.is_none()
                {
                    self.root_function.replace(id)
                } else {
                    None
                };
                let replacement = self.instantiate_composite(kind);
                if matches!(self.root_function, Some(root) if root == id) {
                    self.root_function = previous_root;
                }
                self.arena.replace(placeholder, replacement);
                placeholder
            }
        }
    }

    fn instantiate_composite(&mut self, kind: TypeKind) -> TypeKind {
        match kind {
            TypeKind::Function(function) => TypeKind::Function(self.instantiate_function(function)),
            TypeKind::Table(table) => TypeKind::Table(self.instantiate_table(table)),
            TypeKind::Metatable {
                table,
                metatable,
                name,
            } => {
                let table = self.instantiate_type(table);
                let metatable = self.instantiate_type(metatable);
                TypeKind::Metatable {
                    table,
                    metatable,
                    name,
                }
            }
            TypeKind::TypeFunctionInstance { name, arguments } => {
                let arguments = arguments
                    .into_iter()
                    .map(|ty| self.instantiate_type(ty))
                    .collect();
                TypeKind::TypeFunctionInstance { name, arguments }
            }
            TypeKind::Union(types) => TypeKind::Union(
                types
                    .into_iter()
                    .map(|ty| self.instantiate_type(ty))
                    .collect(),
            ),
            TypeKind::Intersection(types) => TypeKind::Intersection(
                types
                    .into_iter()
                    .map(|ty| self.instantiate_type(ty))
                    .collect(),
            ),
            TypeKind::Negation(ty) => TypeKind::Negation(self.instantiate_type(ty)),
            other => other,
        }
    }

    /// Instantiates a type-pack graph.
    pub fn instantiate_pack(&mut self, id: TypePackId) -> TypePackId {
        let id = self.arena.follow_pack(id);
        if let Some(cached) = self.pack_cache.get(&id) {
            return *cached;
        }
        let result = match self.arena.get_pack(id).clone() {
            TypePackKind::Generic(generic) => self.instantiate_generic_pack(generic),
            TypePackKind::List { types, tail } => {
                let types = types
                    .into_iter()
                    .map(|ty| self.instantiate_type(ty))
                    .collect();
                let tail = tail.map(|tail| self.instantiate_pack(tail));
                self.arena.alloc_pack(TypePackKind::List { types, tail })
            }
            TypePackKind::Variadic { ty } => {
                let ty = self.instantiate_type(ty);
                self.arena.alloc_pack(TypePackKind::Variadic { ty })
            }
            TypePackKind::Bound(_) => id,
            TypePackKind::Free { .. } | TypePackKind::Error => id,
        };
        self.pack_cache.insert(id, result);
        result
    }

    /// Pre-binds a generic type parameter to an explicit substitution.
    pub fn bind_generic(&mut self, generic: &GenericType, replacement: TypeId) {
        self.types
            .insert((generic.name.clone(), generic.level), replacement);
    }

    /// Pre-binds a generic type-pack parameter to an explicit substitution.
    pub fn bind_generic_pack(&mut self, generic: &GenericTypePack, replacement: TypePackId) {
        self.packs
            .insert((generic.name.clone(), generic.level), replacement);
    }

    fn instantiate_function(&mut self, mut function: FunctionType) -> FunctionType {
        if self.function_depth > 0 {
            return self.instantiate_nested_function(function);
        }
        for generic in &function.generics {
            self.instantiate_generic(generic.clone());
        }
        for generic in &function.generic_packs {
            self.instantiate_generic_pack(generic.clone());
        }
        self.function_depth += 1;
        function.arguments = self.instantiate_pack(function.arguments);
        function.returns = self.instantiate_pack(function.returns);
        self.function_depth -= 1;
        function.generics.clear();
        function.generic_packs.clear();
        function
    }

    fn instantiate_nested_function(&mut self, mut function: FunctionType) -> FunctionType {
        let type_keys = function
            .generics
            .iter()
            .map(|generic| {
                let key = (generic.name.clone(), generic.level);
                let replacement = self.arena.alloc(TypeKind::Generic(generic.clone()));
                let previous = self.types.insert(key.clone(), replacement);
                (key, previous)
            })
            .collect::<Vec<_>>();
        let pack_keys = function
            .generic_packs
            .iter()
            .map(|generic| {
                let key = (generic.name.clone(), generic.level);
                let replacement = self
                    .arena
                    .alloc_pack(TypePackKind::Generic(generic.clone()));
                let previous = self.packs.insert(key.clone(), replacement);
                (key, previous)
            })
            .collect::<Vec<_>>();

        self.function_depth += 1;
        function.arguments = self.instantiate_pack(function.arguments);
        function.returns = self.instantiate_pack(function.returns);
        self.function_depth -= 1;

        for (key, previous) in type_keys.into_iter().rev() {
            if let Some(previous) = previous {
                self.types.insert(key, previous);
            } else {
                self.types.remove(&key);
            }
        }
        for (key, previous) in pack_keys.into_iter().rev() {
            if let Some(previous) = previous {
                self.packs.insert(key, previous);
            } else {
                self.packs.remove(&key);
            }
        }
        function
    }

    fn instantiate_table(&mut self, mut table: TableType) -> TableType {
        table.instantiated_type_params = table
            .instantiated_type_params
            .into_iter()
            .map(|ty| self.instantiate_type(ty))
            .collect();
        table.instantiated_type_pack_params = table
            .instantiated_type_pack_params
            .into_iter()
            .map(|pack| self.instantiate_pack(pack))
            .collect();
        // A table's property and indexer types are never the root function being
        // applied: a method-typed field owns its own generics (e.g.
        // `transaction: <R>(...) -> R`). Instantiating them as nested functions
        // preserves those generics instead of stripping them as a root
        // instantiation would, which keeps two instances of the same generic
        // alias unifiable.
        self.function_depth += 1;
        table.map_value_types(|ty| self.instantiate_type(ty));
        self.function_depth -= 1;
        table
    }

    fn instantiate_generic(&mut self, generic: GenericType) -> TypeId {
        let key = (generic.name.clone(), generic.level);
        if let Some(id) = self.types.get(&key) {
            return *id;
        }
        let id = self.arena.alloc(TypeKind::Free(TypeVariable {
            level: self.level,
            name: Some(generic.name),
            lower_bound: None,
            upper_bound: None,
        }));
        self.types.insert(key, id);
        id
    }

    fn instantiate_generic_pack(&mut self, generic: GenericTypePack) -> TypePackId {
        let key = (generic.name.clone(), generic.level);
        if let Some(id) = self.packs.get(&key) {
            return *id;
        }
        let id = self.arena.alloc_pack(TypePackKind::Free {
            level: self.level,
            name: Some(generic.name),
        });
        self.packs.insert(key, id);
        id
    }
}

fn dedup_generics(generics: &mut Vec<GenericType>) {
    let mut seen = BTreeSet::new();
    generics.retain(|generic| seen.insert((generic.name.clone(), generic.level)));
}

fn dedup_generic_packs(generics: &mut Vec<GenericTypePack>) {
    let mut seen = BTreeSet::new();
    generics.retain(|generic| seen.insert((generic.name.clone(), generic.level)));
}

/// Generalizes unbound frees in a checked function value into function
/// generics, independent of level.
///
/// This is for function values after their body has been checked. During body
/// checking recursive references must still see the ungeneralized function.
pub fn generalize_function_frees(arena: &mut Arena, id: TypeId) -> TypeId {
    FunctionFreeGeneralizer::new(arena).generalize_type(id)
}

/// Like `generalize_function_frees`, but turns genuinely-unconstrained frees
/// into `unknown` rather than quantifying them. Nonstrict mode does not infer
/// polymorphic signatures: an unconstrained parameter or result is `unknown`.
pub fn generalize_function_frees_to_unknown(arena: &mut Arena, id: TypeId) -> TypeId {
    let mut g = FunctionFreeGeneralizer::new(arena);
    g.frees_to_unknown = true;
    g.generalize_type(id)
}

/// Generalizes unbound frees in a checked function declaration into generics on
/// the declaration itself.
///
/// Unlike `generalize_function_frees`, this treats function-valued parameter
/// shapes as part of the outer declaration signature, so a declaration inferred
/// as `((A) -> B, A) -> B` stores the `A` and `B` correlation on the outer
/// function rather than introducing a fresh generic function parameter.
pub fn generalize_function_signature_frees(arena: &mut Arena, id: TypeId) -> TypeId {
    FunctionFreeGeneralizer::new(arena).generalize_flattened_function_type(id)
}

/// Resolves one-sided free-variable bounds inside a function query return surface.
///
/// This is query-only presentation: the solver keeps the original bounded
/// frees for value-flow constraints, while `requireType` reports the concrete
/// bound the solver learned. Function parameter positions stay untouched
/// because resolving contravariant bounds can erase annotated input shape.
pub fn resolve_function_free_bounds_for_query(arena: &mut Arena, id: TypeId) -> TypeId {
    QueryFreeBoundResolver::new(arena).resolve_type(id)
}

pub fn function_signature_has_callback_free_correlation(arena: &Arena, id: TypeId) -> bool {
    SignatureCorrelationFinder::new(arena).has_callback_free_correlation(id)
}

struct QueryFreeBoundResolver<'a> {
    arena: &'a mut Arena,
    types: BTreeMap<TypeId, TypeId>,
    packs: BTreeMap<TypePackId, TypePackId>,
}

impl<'a> QueryFreeBoundResolver<'a> {
    fn new(arena: &'a mut Arena) -> Self {
        Self {
            arena,
            types: BTreeMap::new(),
            packs: BTreeMap::new(),
        }
    }

    fn resolve_type(&mut self, id: TypeId) -> TypeId {
        let id = self.arena.follow(id);
        if let Some(resolved) = self.types.get(&id) {
            return *resolved;
        }
        match self.arena.get(id).clone() {
            TypeKind::Free(variable) => match (variable.lower_bound, variable.upper_bound) {
                (None, Some(bound)) | (Some(bound), None) => {
                    // A bound chain can cycle back through this free
                    // (fuzz-found stack overflow); mark it in-progress so the
                    // cycle resolves to the free itself instead of recursing
                    // unboundedly.
                    self.types.insert(id, id);
                    let resolved = self.resolve_type(bound);
                    self.types.insert(id, resolved);
                    resolved
                }
                _ => id,
            },
            TypeKind::Bound(bound) => {
                // Same cycle guard as the one-sided free arm.
                self.types.insert(id, id);
                let resolved = self.resolve_type(bound);
                self.types.insert(id, resolved);
                resolved
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => id,
            kind => {
                let placeholder = self.arena.alloc(TypeKind::Blocked(BlockedType::default()));
                self.types.insert(id, placeholder);
                let resolved = self.resolve_composite(kind);
                self.arena.replace(placeholder, resolved);
                placeholder
            }
        }
    }

    fn resolve_composite(&mut self, kind: TypeKind) -> TypeKind {
        match kind {
            TypeKind::Function(mut function) => {
                function.returns = self.resolve_pack(function.returns);
                TypeKind::Function(function)
            }
            TypeKind::Table(table) => TypeKind::Table(self.resolve_table(table)),
            TypeKind::Metatable {
                table,
                metatable,
                name,
            } => TypeKind::Metatable {
                table: self.resolve_type(table),
                metatable: self.resolve_type(metatable),
                name,
            },
            TypeKind::TypeFunctionInstance { name, arguments } => TypeKind::TypeFunctionInstance {
                name,
                arguments: arguments
                    .into_iter()
                    .map(|ty| self.resolve_type(ty))
                    .collect(),
            },
            TypeKind::Union(types) => {
                TypeKind::Union(types.into_iter().map(|ty| self.resolve_type(ty)).collect())
            }
            TypeKind::Intersection(types) => {
                TypeKind::Intersection(types.into_iter().map(|ty| self.resolve_type(ty)).collect())
            }
            TypeKind::Negation(ty) => TypeKind::Negation(self.resolve_type(ty)),
            other => other,
        }
    }

    fn resolve_table(&mut self, mut table: TableType) -> TableType {
        table.instantiated_type_params = table
            .instantiated_type_params
            .into_iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        table.instantiated_type_pack_params = table
            .instantiated_type_pack_params
            .into_iter()
            .map(|pack| self.resolve_pack(pack))
            .collect();
        table.map_value_types(|ty| self.resolve_type(ty));
        table
    }

    fn resolve_pack(&mut self, id: TypePackId) -> TypePackId {
        let id = self.arena.follow_pack(id);
        if let Some(resolved) = self.packs.get(&id) {
            return *resolved;
        }
        let resolved = match self.arena.get_pack(id).clone() {
            TypePackKind::List { types, tail } => {
                let placeholder = self.arena.alloc_pack(TypePackKind::Error);
                self.packs.insert(id, placeholder);
                let types = types.into_iter().map(|ty| self.resolve_type(ty)).collect();
                let tail = tail.map(|tail| self.resolve_pack(tail));
                self.arena
                    .replace_pack(placeholder, TypePackKind::List { types, tail });
                placeholder
            }
            TypePackKind::Variadic { ty } => {
                let ty = self.resolve_type(ty);
                self.arena.alloc_pack(TypePackKind::Variadic { ty })
            }
            TypePackKind::Bound(bound) => self.resolve_pack(bound),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => id,
        };
        self.packs.insert(id, resolved);
        resolved
    }
}

struct SignatureCorrelationFinder<'a> {
    arena: &'a Arena,
}

impl<'a> SignatureCorrelationFinder<'a> {
    fn new(arena: &'a Arena) -> Self {
        Self { arena }
    }

    fn has_callback_free_correlation(&self, id: TypeId) -> bool {
        let TypeKind::Function(function) = self.arena.get(self.arena.follow(id)) else {
            return false;
        };
        let mut outer_types = BTreeSet::new();
        let mut outer_packs = BTreeSet::new();
        self.collect_unbound_frees_in_pack(
            function.arguments,
            false,
            &mut outer_types,
            &mut outer_packs,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        );
        self.collect_unbound_frees_in_pack(
            function.returns,
            false,
            &mut outer_types,
            &mut outer_packs,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        );
        if outer_types.is_empty() && outer_packs.is_empty() {
            return false;
        }
        self.argument_pack_has_callback_free_intersection(
            function.arguments,
            &outer_types,
            &outer_packs,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        )
    }

    fn argument_pack_has_callback_free_intersection(
        &self,
        pack: TypePackId,
        outer_types: &BTreeSet<TypeId>,
        outer_packs: &BTreeSet<TypePackId>,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.arena.get_pack(pack) {
            TypePackKind::List { types, tail } => {
                types.iter().any(|ty| {
                    self.type_has_callback_free_intersection(
                        *ty,
                        outer_types,
                        outer_packs,
                        seen_types,
                        seen_packs,
                    )
                }) || tail.is_some_and(|tail| {
                    self.argument_pack_has_callback_free_intersection(
                        tail,
                        outer_types,
                        outer_packs,
                        seen_types,
                        seen_packs,
                    )
                })
            }
            TypePackKind::Variadic { ty } => self.type_has_callback_free_intersection(
                *ty,
                outer_types,
                outer_packs,
                seen_types,
                seen_packs,
            ),
            TypePackKind::Bound(bound) => self.argument_pack_has_callback_free_intersection(
                *bound,
                outer_types,
                outer_packs,
                seen_types,
                seen_packs,
            ),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }

    #[allow(clippy::only_used_in_recursion)]
    fn type_has_callback_free_intersection(
        &self,
        ty: TypeId,
        outer_types: &BTreeSet<TypeId>,
        outer_packs: &BTreeSet<TypePackId>,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return false;
        }
        match self.arena.get(ty) {
            TypeKind::Function(function) => {
                let mut callback_types = BTreeSet::new();
                let mut callback_packs = BTreeSet::new();
                self.collect_unbound_frees_in_pack(
                    function.arguments,
                    true,
                    &mut callback_types,
                    &mut callback_packs,
                    &mut BTreeSet::new(),
                    &mut BTreeSet::new(),
                );
                self.collect_unbound_frees_in_pack(
                    function.returns,
                    true,
                    &mut callback_types,
                    &mut callback_packs,
                    &mut BTreeSet::new(),
                    &mut BTreeSet::new(),
                );
                callback_types.iter().any(|ty| outer_types.contains(ty))
                    || callback_packs.iter().any(|pack| outer_packs.contains(pack))
            }
            TypeKind::Table(table) => {
                table.instantiated_type_params.iter().any(|ty| {
                    self.type_has_callback_free_intersection(
                        *ty,
                        outer_types,
                        outer_packs,
                        seen_types,
                        seen_packs,
                    )
                }) || table.properties.values().any(|property| {
                    self.type_has_callback_free_intersection(
                        property.ty,
                        outer_types,
                        outer_packs,
                        seen_types,
                        seen_packs,
                    )
                }) || table.indexer.as_ref().is_some_and(|indexer| {
                    self.type_has_callback_free_intersection(
                        indexer.key,
                        outer_types,
                        outer_packs,
                        seen_types,
                        seen_packs,
                    ) || self.type_has_callback_free_intersection(
                        indexer.value,
                        outer_types,
                        outer_packs,
                        seen_types,
                        seen_packs,
                    )
                })
            }
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                properties.values().any(|property| {
                    self.type_has_callback_free_intersection(
                        property.ty,
                        outer_types,
                        outer_packs,
                        seen_types,
                        seen_packs,
                    )
                }) || indexer.as_ref().is_some_and(|indexer| {
                    self.type_has_callback_free_intersection(
                        indexer.key,
                        outer_types,
                        outer_packs,
                        seen_types,
                        seen_packs,
                    ) || self.type_has_callback_free_intersection(
                        indexer.value,
                        outer_types,
                        outer_packs,
                        seen_types,
                        seen_packs,
                    )
                })
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_has_callback_free_intersection(
                    *table,
                    outer_types,
                    outer_packs,
                    seen_types,
                    seen_packs,
                ) || self.type_has_callback_free_intersection(
                    *metatable,
                    outer_types,
                    outer_packs,
                    seen_types,
                    seen_packs,
                )
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => arguments.iter().any(|ty| {
                self.type_has_callback_free_intersection(
                    *ty,
                    outer_types,
                    outer_packs,
                    seen_types,
                    seen_packs,
                )
            }),
            TypeKind::Negation(inner) => self.type_has_callback_free_intersection(
                *inner,
                outer_types,
                outer_packs,
                seen_types,
                seen_packs,
            ),
            TypeKind::Bound(bound) => self.type_has_callback_free_intersection(
                *bound,
                outer_types,
                outer_packs,
                seen_types,
                seen_packs,
            ),
            TypeKind::Free(_)
            | TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }

    fn collect_unbound_frees_in_pack(
        &self,
        pack: TypePackId,
        include_nested_functions: bool,
        types: &mut BTreeSet<TypeId>,
        packs: &mut BTreeSet<TypePackId>,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) {
        let pack = self.arena.follow_pack(pack);
        if !seen_packs.insert(pack) {
            return;
        }
        match self.arena.get_pack(pack) {
            TypePackKind::Free { .. } => {
                packs.insert(pack);
            }
            TypePackKind::List { types: items, tail } => {
                for ty in items {
                    self.collect_unbound_frees_in_type(
                        *ty,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                }
                if let Some(tail) = tail {
                    self.collect_unbound_frees_in_pack(
                        *tail,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypePackKind::Variadic { ty } => self.collect_unbound_frees_in_type(
                *ty,
                include_nested_functions,
                types,
                packs,
                seen_types,
                seen_packs,
            ),
            TypePackKind::Bound(bound) => self.collect_unbound_frees_in_pack(
                *bound,
                include_nested_functions,
                types,
                packs,
                seen_types,
                seen_packs,
            ),
            TypePackKind::Generic(_) | TypePackKind::Error => {}
        }
    }

    fn collect_unbound_frees_in_type(
        &self,
        ty: TypeId,
        include_nested_functions: bool,
        types: &mut BTreeSet<TypeId>,
        packs: &mut BTreeSet<TypePackId>,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return;
        }
        match self.arena.get(ty) {
            TypeKind::Free(variable)
                if variable.lower_bound.is_none() && variable.upper_bound.is_none() =>
            {
                types.insert(ty);
            }
            TypeKind::Function(function) => {
                if include_nested_functions {
                    self.collect_unbound_frees_in_pack(
                        function.arguments,
                        true,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                    self.collect_unbound_frees_in_pack(
                        function.returns,
                        true,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Table(table) => {
                for ty in &table.instantiated_type_params {
                    self.collect_unbound_frees_in_type(
                        *ty,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                }
                for property in table.properties.values() {
                    self.collect_unbound_frees_in_type(
                        property.ty,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                }
                if let Some(indexer) = &table.indexer {
                    self.collect_unbound_frees_in_type(
                        indexer.key,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                    self.collect_unbound_frees_in_type(
                        indexer.value,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                for property in properties.values() {
                    self.collect_unbound_frees_in_type(
                        property.ty,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                }
                if let Some(indexer) = indexer {
                    self.collect_unbound_frees_in_type(
                        indexer.key,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                    self.collect_unbound_frees_in_type(
                        indexer.value,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.collect_unbound_frees_in_type(
                    *table,
                    include_nested_functions,
                    types,
                    packs,
                    seen_types,
                    seen_packs,
                );
                self.collect_unbound_frees_in_type(
                    *metatable,
                    include_nested_functions,
                    types,
                    packs,
                    seen_types,
                    seen_packs,
                );
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => {
                for ty in arguments {
                    self.collect_unbound_frees_in_type(
                        *ty,
                        include_nested_functions,
                        types,
                        packs,
                        seen_types,
                        seen_packs,
                    );
                }
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => self
                .collect_unbound_frees_in_type(
                    *inner,
                    include_nested_functions,
                    types,
                    packs,
                    seen_types,
                    seen_packs,
                ),
            TypeKind::Free(_)
            | TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => {}
        }
    }
}

struct FunctionFreeGeneralizer<'a> {
    arena: &'a mut Arena,
    types: BTreeMap<TypeId, TypeId>,
    packs: BTreeMap<TypePackId, TypePackId>,
    generics: Vec<GenericType>,
    generic_packs: Vec<GenericTypePack>,
    frees_to_unknown: bool,
    /// Free type variables this run replaced (keyed by the followed original id).
    /// A named generic table keeps its body shared rather than copied, so a body
    /// reference to a parameter free variable must be remapped to the same
    /// replacement the parameter slot received, or the two would diverge.
    free_substitutions: BTreeMap<TypeId, TypeId>,
    free_pack_substitutions: BTreeMap<TypePackId, TypePackId>,
}

impl<'a> FunctionFreeGeneralizer<'a> {
    fn new(arena: &'a mut Arena) -> Self {
        Self {
            arena,
            types: BTreeMap::new(),
            packs: BTreeMap::new(),
            generics: Vec::new(),
            generic_packs: Vec::new(),
            frees_to_unknown: false,
            free_substitutions: BTreeMap::new(),
            free_pack_substitutions: BTreeMap::new(),
        }
    }

    fn generalize_type(&mut self, id: TypeId) -> TypeId {
        let id = self.arena.follow(id);
        if let Some(generalized) = self.types.get(&id) {
            return *generalized;
        }

        let generalized = match self.arena.get(id).clone() {
            TypeKind::Free(variable)
                if variable.lower_bound.is_none() && variable.upper_bound.is_none() =>
            {
                let replacement = if self.frees_to_unknown {
                    self.arena.primitives().unknown
                } else {
                    let generic = GenericType {
                        name: type_generic_name(self.generics.len()),
                        level: variable.level,
                    };
                    let ty = self.arena.alloc(TypeKind::Generic(generic.clone()));
                    self.generics.push(generic);
                    ty
                };
                self.free_substitutions.insert(id, replacement);
                replacement
            }
            TypeKind::Function(function) => {
                let empty_pack = self.arena.empty_pack();
                let copy = self.arena.alloc(TypeKind::Function(FunctionType::new(
                    empty_pack, empty_pack,
                )));
                self.types.insert(id, copy);
                let function = self.generalize_function(function);
                self.arena.replace(copy, TypeKind::Function(function));
                copy
            }
            TypeKind::Table(table) => {
                let copy = self
                    .arena
                    .alloc(TypeKind::Table(TableType::new(table.state)));
                self.types.insert(id, copy);
                let table = self.generalize_table(id, copy, table);
                self.arena.replace(copy, TypeKind::Table(table));
                copy
            }
            TypeKind::Metatable {
                table,
                metatable,
                name,
            } => self.generalize_metatable(id, table, metatable, name),
            TypeKind::TypeFunctionInstance { name, arguments } => {
                let copy = self.arena.alloc(TypeKind::TypeFunctionInstance {
                    name: name.clone(),
                    arguments: Vec::new(),
                });
                self.types.insert(id, copy);
                let arguments = arguments
                    .into_iter()
                    .map(|ty| self.generalize_type(ty))
                    .collect();
                self.arena
                    .replace(copy, TypeKind::TypeFunctionInstance { name, arguments });
                copy
            }
            TypeKind::Union(types) => {
                let copy = self.arena.alloc(TypeKind::Union(Vec::new()));
                self.types.insert(id, copy);
                let types = types
                    .into_iter()
                    .map(|ty| self.generalize_type(ty))
                    .collect();
                self.arena.replace(copy, TypeKind::Union(types));
                copy
            }
            TypeKind::Intersection(types) => {
                let copy = self.arena.alloc(TypeKind::Intersection(Vec::new()));
                self.types.insert(id, copy);
                let types = types
                    .into_iter()
                    .map(|ty| self.generalize_type(ty))
                    .collect();
                self.arena.replace(copy, TypeKind::Intersection(types));
                copy
            }
            TypeKind::Negation(ty) => {
                let copy = self.arena.alloc(TypeKind::Negation(ty));
                self.types.insert(id, copy);
                let ty = self.generalize_type(ty);
                self.arena.replace(copy, TypeKind::Negation(ty));
                copy
            }
            TypeKind::Bound(_) => self.arena.primitives().error,
            TypeKind::Free(_)
            | TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => id,
        };

        self.types.insert(id, generalized);
        generalized
    }

    fn generalize_pack(&mut self, id: TypePackId) -> TypePackId {
        let id = self.arena.follow_pack(id);
        if let Some(generalized) = self.packs.get(&id) {
            return *generalized;
        }

        let generalized = match self.arena.get_pack(id).clone() {
            TypePackKind::Free { level, .. } => {
                let generic = GenericTypePack {
                    name: pack_generic_name(self.generic_packs.len()),
                    level,
                };
                let pack = self
                    .arena
                    .alloc_pack(TypePackKind::Generic(generic.clone()));
                self.generic_packs.push(generic);
                self.free_pack_substitutions.insert(id, pack);
                pack
            }
            TypePackKind::List { types, tail } => {
                let copy = self.arena.alloc_pack(TypePackKind::List {
                    types: Vec::new(),
                    tail: None,
                });
                self.packs.insert(id, copy);
                let types = types
                    .into_iter()
                    .map(|ty| self.generalize_type(ty))
                    .collect();
                let tail = tail.map(|tail| self.generalize_pack(tail));
                self.arena
                    .replace_pack(copy, TypePackKind::List { types, tail });
                copy
            }
            TypePackKind::Variadic { ty } => {
                let copy = self.arena.alloc_pack(TypePackKind::Variadic { ty });
                self.packs.insert(id, copy);
                let ty = self.generalize_type(ty);
                self.arena.replace_pack(copy, TypePackKind::Variadic { ty });
                copy
            }
            TypePackKind::Bound(_) => id,
            TypePackKind::Generic(_) | TypePackKind::Error => id,
        };

        self.packs.insert(id, generalized);
        generalized
    }

    fn generalize_function(&mut self, mut function: FunctionType) -> FunctionType {
        let generic_start = self.generics.len();
        let generic_pack_start = self.generic_packs.len();
        function.arguments = self.generalize_pack(function.arguments);
        function.returns = self.generalize_pack(function.returns);
        function
            .generics
            .extend(self.generics[generic_start..].iter().cloned());
        function
            .generic_packs
            .extend(self.generic_packs[generic_pack_start..].iter().cloned());
        dedup_generics(&mut function.generics);
        dedup_generic_packs(&mut function.generic_packs);
        function
    }

    fn generalize_flattened_function_type(&mut self, id: TypeId) -> TypeId {
        let generalized = self.generalize_flattened_type(id);
        let TypeKind::Function(mut function) = self.arena.get(generalized).clone() else {
            return generalized;
        };
        function.generics.extend(self.generics.iter().cloned());
        function
            .generic_packs
            .extend(self.generic_packs.iter().cloned());
        dedup_generics(&mut function.generics);
        dedup_generic_packs(&mut function.generic_packs);
        self.arena
            .replace(generalized, TypeKind::Function(function));
        generalized
    }

    fn generalize_flattened_type(&mut self, id: TypeId) -> TypeId {
        let id = self.arena.follow(id);
        if let Some(generalized) = self.types.get(&id) {
            return *generalized;
        }

        let generalized = match self.arena.get(id).clone() {
            TypeKind::Free(variable)
                if variable.lower_bound.is_none() && variable.upper_bound.is_none() =>
            {
                let generic = GenericType {
                    name: type_generic_name(self.generics.len()),
                    level: variable.level,
                };
                let ty = self.arena.alloc(TypeKind::Generic(generic.clone()));
                self.generics.push(generic);
                self.free_substitutions.insert(id, ty);
                ty
            }
            TypeKind::Function(mut function) => {
                let empty_pack = self.arena.empty_pack();
                let copy = self.arena.alloc(TypeKind::Function(FunctionType::new(
                    empty_pack, empty_pack,
                )));
                self.types.insert(id, copy);
                function.arguments = self.generalize_flattened_pack(function.arguments);
                function.returns = self.generalize_flattened_pack(function.returns);
                self.arena.replace(copy, TypeKind::Function(function));
                copy
            }
            TypeKind::Table(mut table) => {
                let copy = self
                    .arena
                    .alloc(TypeKind::Table(TableType::new(table.state)));
                self.types.insert(id, copy);
                if table.name.is_some() {
                    table.instantiated_type_params = table
                        .instantiated_type_params
                        .into_iter()
                        .map(|ty| self.generalize_flattened_type(ty))
                        .collect();
                    table.instantiated_type_pack_params = table
                        .instantiated_type_pack_params
                        .into_iter()
                        .map(|pack| self.generalize_flattened_pack(pack))
                        .collect();
                    self.remap_generalized_named_table_body(id, copy, &mut table);
                    self.arena.replace(copy, TypeKind::Table(table));
                    return copy;
                }
                table.instantiated_type_params = table
                    .instantiated_type_params
                    .into_iter()
                    .map(|ty| self.generalize_flattened_type(ty))
                    .collect();
                table.instantiated_type_pack_params = table
                    .instantiated_type_pack_params
                    .into_iter()
                    .map(|pack| self.generalize_flattened_pack(pack))
                    .collect();
                table.map_value_types(|ty| self.generalize_flattened_type(ty));
                self.arena.replace(copy, TypeKind::Table(table));
                copy
            }
            TypeKind::Metatable {
                table,
                metatable,
                name,
            } => self.generalize_flattened_metatable(id, table, metatable, name),
            TypeKind::TypeFunctionInstance { name, arguments } => {
                let copy = self.arena.alloc(TypeKind::TypeFunctionInstance {
                    name: name.clone(),
                    arguments: Vec::new(),
                });
                self.types.insert(id, copy);
                let arguments = arguments
                    .into_iter()
                    .map(|ty| self.generalize_flattened_type(ty))
                    .collect();
                self.arena
                    .replace(copy, TypeKind::TypeFunctionInstance { name, arguments });
                copy
            }
            TypeKind::Union(types) => {
                let copy = self.arena.alloc(TypeKind::Union(Vec::new()));
                self.types.insert(id, copy);
                let types = types
                    .into_iter()
                    .map(|ty| self.generalize_flattened_type(ty))
                    .collect();
                self.arena.replace(copy, TypeKind::Union(types));
                copy
            }
            TypeKind::Intersection(types) => {
                let copy = self.arena.alloc(TypeKind::Intersection(Vec::new()));
                self.types.insert(id, copy);
                let types = types
                    .into_iter()
                    .map(|ty| self.generalize_flattened_type(ty))
                    .collect();
                self.arena.replace(copy, TypeKind::Intersection(types));
                copy
            }
            TypeKind::Negation(ty) => {
                let copy = self.arena.alloc(TypeKind::Negation(ty));
                self.types.insert(id, copy);
                let ty = self.generalize_flattened_type(ty);
                self.arena.replace(copy, TypeKind::Negation(ty));
                copy
            }
            TypeKind::Bound(_) => self.arena.primitives().error,
            TypeKind::Free(_)
            | TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => id,
        };

        self.types.insert(id, generalized);
        generalized
    }

    fn generalize_flattened_pack(&mut self, id: TypePackId) -> TypePackId {
        let id = self.arena.follow_pack(id);
        if let Some(generalized) = self.packs.get(&id) {
            return *generalized;
        }

        let generalized = match self.arena.get_pack(id).clone() {
            TypePackKind::Free { level, .. } => {
                let generic = GenericTypePack {
                    name: pack_generic_name(self.generic_packs.len()),
                    level,
                };
                let pack = self
                    .arena
                    .alloc_pack(TypePackKind::Generic(generic.clone()));
                self.generic_packs.push(generic);
                self.free_pack_substitutions.insert(id, pack);
                pack
            }
            TypePackKind::List { types, tail } => {
                let copy = self.arena.alloc_pack(TypePackKind::List {
                    types: Vec::new(),
                    tail: None,
                });
                self.packs.insert(id, copy);
                let types = types
                    .into_iter()
                    .map(|ty| self.generalize_flattened_type(ty))
                    .collect();
                let tail = tail.map(|tail| self.generalize_flattened_pack(tail));
                self.arena
                    .replace_pack(copy, TypePackKind::List { types, tail });
                copy
            }
            TypePackKind::Variadic { ty } => {
                let copy = self.arena.alloc_pack(TypePackKind::Variadic { ty });
                self.packs.insert(id, copy);
                let ty = self.generalize_flattened_type(ty);
                self.arena.replace_pack(copy, TypePackKind::Variadic { ty });
                copy
            }
            TypePackKind::Bound(_) => id,
            TypePackKind::Generic(_) | TypePackKind::Error => id,
        };

        self.packs.insert(id, generalized);
        generalized
    }

    fn generalize_metatable(
        &mut self,
        id: TypeId,
        table: TypeId,
        metatable: TypeId,
        name: Option<String>,
    ) -> TypeId {
        let copy = self.arena.alloc(TypeKind::Blocked(BlockedType::default()));
        self.types.insert(id, copy);
        let table = self.generalize_type(table);
        let followed_metatable = self.arena.follow(metatable);
        let metatable = self
            .types
            .get(&followed_metatable)
            .copied()
            .unwrap_or(metatable);
        // Keep the metatable payload identity. Relational metamethod checks use
        // payload equality to decide whether two constructed values share a
        // metatable. If the payload is also part of the copied graph, point at
        // that copy so nested constructors keep their class/instance link.
        self.arena.replace(
            copy,
            TypeKind::Metatable {
                table,
                metatable,
                name,
            },
        );
        copy
    }

    fn generalize_flattened_metatable(
        &mut self,
        id: TypeId,
        table: TypeId,
        metatable: TypeId,
        name: Option<String>,
    ) -> TypeId {
        let copy = self.arena.alloc(TypeKind::Blocked(BlockedType::default()));
        self.types.insert(id, copy);
        let table = self.generalize_flattened_type(table);
        let followed_metatable = self.arena.follow(metatable);
        let metatable = self
            .types
            .get(&followed_metatable)
            .copied()
            .unwrap_or(metatable);
        self.arena.replace(
            copy,
            TypeKind::Metatable {
                table,
                metatable,
                name,
            },
        );
        copy
    }

    fn generalize_table(
        &mut self,
        original_id: TypeId,
        copy: TypeId,
        mut table: TableType,
    ) -> TableType {
        table.instantiated_type_params = table
            .instantiated_type_params
            .into_iter()
            .map(|ty| self.generalize_type(ty))
            .collect();
        table.instantiated_type_pack_params = table
            .instantiated_type_pack_params
            .into_iter()
            .map(|pack| self.generalize_pack(pack))
            .collect();
        if table.name.is_some() {
            self.remap_generalized_named_table_body(original_id, copy, &mut table);
            return table;
        }
        table.map_value_types(|ty| self.generalize_type(ty));
        table
    }

    /// Rewrites a named generic table's shared body so references to a
    /// parameter free variable use the same generic the parameter slot was
    /// generalized to. The body is left shared (not deep-generalized) when no
    /// parameter free variable was generalized, preserving the structural
    /// stability that named instances rely on for subtype comparison.
    fn remap_generalized_named_table_body(
        &mut self,
        original_id: TypeId,
        copy: TypeId,
        table: &mut TableType,
    ) {
        if self.free_substitutions.is_empty() && self.free_pack_substitutions.is_empty() {
            return;
        }
        let type_subst = self.free_substitutions.clone();
        let pack_subst = self.free_pack_substitutions.clone();
        let mut memo = BTreeMap::new();
        // Body self-references resolve to the in-progress generalized copy so the
        // rewritten body stays recursive rather than pointing at the original.
        memo.insert(self.arena.follow(original_id), copy);
        let mut pack_memo = BTreeMap::new();
        for property in table.properties.values_mut() {
            property.ty = substitute_named_body_type(
                self.arena,
                &type_subst,
                &pack_subst,
                &mut memo,
                &mut pack_memo,
                property.ty,
            );
            if let Some(write_ty) = property.write_ty {
                property.write_ty = Some(substitute_named_body_type(
                    self.arena,
                    &type_subst,
                    &pack_subst,
                    &mut memo,
                    &mut pack_memo,
                    write_ty,
                ));
            }
        }
        if let Some(indexer) = &mut table.indexer {
            indexer.key = substitute_named_body_type(
                self.arena,
                &type_subst,
                &pack_subst,
                &mut memo,
                &mut pack_memo,
                indexer.key,
            );
            indexer.value = substitute_named_body_type(
                self.arena,
                &type_subst,
                &pack_subst,
                &mut memo,
                &mut pack_memo,
                indexer.value,
            );
        }
    }
}

/// Copies a type graph, replacing free variables listed in `type_subst` (and
/// free packs in `pack_subst`) with their generalized replacements. Other nodes
/// are reproduced structurally; `memo`/`pack_memo` keep shared and recursive
/// subgraphs consistent.
fn substitute_named_body_type(
    arena: &mut Arena,
    type_subst: &BTreeMap<TypeId, TypeId>,
    pack_subst: &BTreeMap<TypePackId, TypePackId>,
    memo: &mut BTreeMap<TypeId, TypeId>,
    pack_memo: &mut BTreeMap<TypePackId, TypePackId>,
    id: TypeId,
) -> TypeId {
    let id = arena.follow(id);
    if let Some(mapped) = memo.get(&id) {
        return *mapped;
    }
    if let Some(mapped) = type_subst.get(&id) {
        return *mapped;
    }
    match arena.get(id).clone() {
        TypeKind::Free(_)
        | TypeKind::Generic(_)
        | TypeKind::Primitive(_)
        | TypeKind::Singleton(_)
        | TypeKind::Extern { .. }
        | TypeKind::Blocked(_)
        | TypeKind::Error
        | TypeKind::Unknown
        | TypeKind::Never
        | TypeKind::Any => id,
        TypeKind::Bound(bound) => {
            let result =
                substitute_named_body_type(arena, type_subst, pack_subst, memo, pack_memo, bound);
            memo.insert(id, result);
            result
        }
        TypeKind::Function(mut function) => {
            let placeholder = arena.alloc(TypeKind::Blocked(BlockedType::default()));
            memo.insert(id, placeholder);
            function.arguments = substitute_named_body_pack(
                arena,
                type_subst,
                pack_subst,
                memo,
                pack_memo,
                function.arguments,
            );
            function.returns = substitute_named_body_pack(
                arena,
                type_subst,
                pack_subst,
                memo,
                pack_memo,
                function.returns,
            );
            arena.replace(placeholder, TypeKind::Function(function));
            placeholder
        }
        TypeKind::Table(mut table) => {
            let placeholder = arena.alloc(TypeKind::Blocked(BlockedType::default()));
            memo.insert(id, placeholder);
            table.instantiated_type_params = table
                .instantiated_type_params
                .into_iter()
                .map(|ty| {
                    substitute_named_body_type(arena, type_subst, pack_subst, memo, pack_memo, ty)
                })
                .collect();
            table.instantiated_type_pack_params = table
                .instantiated_type_pack_params
                .into_iter()
                .map(|pack| {
                    substitute_named_body_pack(arena, type_subst, pack_subst, memo, pack_memo, pack)
                })
                .collect();
            table.properties = table
                .properties
                .into_iter()
                .map(|(name, mut property)| {
                    property.ty = substitute_named_body_type(
                        arena,
                        type_subst,
                        pack_subst,
                        memo,
                        pack_memo,
                        property.ty,
                    );
                    property.write_ty = property.write_ty.map(|ty| {
                        substitute_named_body_type(
                            arena, type_subst, pack_subst, memo, pack_memo, ty,
                        )
                    });
                    (name, property)
                })
                .collect();
            table.indexer = table.indexer.map(|indexer| TableIndexer {
                key: substitute_named_body_type(
                    arena,
                    type_subst,
                    pack_subst,
                    memo,
                    pack_memo,
                    indexer.key,
                ),
                value: substitute_named_body_type(
                    arena,
                    type_subst,
                    pack_subst,
                    memo,
                    pack_memo,
                    indexer.value,
                ),
                read_only: indexer.read_only,
            });
            arena.replace(placeholder, TypeKind::Table(table));
            placeholder
        }
        TypeKind::Metatable {
            table,
            metatable,
            name,
        } => {
            let placeholder = arena.alloc(TypeKind::Blocked(BlockedType::default()));
            memo.insert(id, placeholder);
            let table =
                substitute_named_body_type(arena, type_subst, pack_subst, memo, pack_memo, table);
            let metatable = substitute_named_body_type(
                arena, type_subst, pack_subst, memo, pack_memo, metatable,
            );
            arena.replace(
                placeholder,
                TypeKind::Metatable {
                    table,
                    metatable,
                    name,
                },
            );
            placeholder
        }
        TypeKind::TypeFunctionInstance { name, arguments } => {
            let placeholder = arena.alloc(TypeKind::Blocked(BlockedType::default()));
            memo.insert(id, placeholder);
            let arguments = arguments
                .into_iter()
                .map(|ty| {
                    substitute_named_body_type(arena, type_subst, pack_subst, memo, pack_memo, ty)
                })
                .collect();
            arena.replace(
                placeholder,
                TypeKind::TypeFunctionInstance { name, arguments },
            );
            placeholder
        }
        TypeKind::Union(types) => {
            let placeholder = arena.alloc(TypeKind::Blocked(BlockedType::default()));
            memo.insert(id, placeholder);
            let types = types
                .into_iter()
                .map(|ty| {
                    substitute_named_body_type(arena, type_subst, pack_subst, memo, pack_memo, ty)
                })
                .collect();
            arena.replace(placeholder, TypeKind::Union(types));
            placeholder
        }
        TypeKind::Intersection(types) => {
            let placeholder = arena.alloc(TypeKind::Blocked(BlockedType::default()));
            memo.insert(id, placeholder);
            let types = types
                .into_iter()
                .map(|ty| {
                    substitute_named_body_type(arena, type_subst, pack_subst, memo, pack_memo, ty)
                })
                .collect();
            arena.replace(placeholder, TypeKind::Intersection(types));
            placeholder
        }
        TypeKind::Negation(inner) => {
            let placeholder = arena.alloc(TypeKind::Blocked(BlockedType::default()));
            memo.insert(id, placeholder);
            let inner =
                substitute_named_body_type(arena, type_subst, pack_subst, memo, pack_memo, inner);
            arena.replace(placeholder, TypeKind::Negation(inner));
            placeholder
        }
    }
}

fn substitute_named_body_pack(
    arena: &mut Arena,
    type_subst: &BTreeMap<TypeId, TypeId>,
    pack_subst: &BTreeMap<TypePackId, TypePackId>,
    memo: &mut BTreeMap<TypeId, TypeId>,
    pack_memo: &mut BTreeMap<TypePackId, TypePackId>,
    id: TypePackId,
) -> TypePackId {
    let id = arena.follow_pack(id);
    if let Some(mapped) = pack_memo.get(&id) {
        return *mapped;
    }
    if let Some(mapped) = pack_subst.get(&id) {
        return *mapped;
    }
    match arena.get_pack(id).clone() {
        TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => id,
        TypePackKind::Bound(bound) => {
            let result =
                substitute_named_body_pack(arena, type_subst, pack_subst, memo, pack_memo, bound);
            pack_memo.insert(id, result);
            result
        }
        TypePackKind::List { types, tail } => {
            let placeholder = arena.alloc_pack(TypePackKind::List {
                types: Vec::new(),
                tail: None,
            });
            pack_memo.insert(id, placeholder);
            let types = types
                .into_iter()
                .map(|ty| {
                    substitute_named_body_type(arena, type_subst, pack_subst, memo, pack_memo, ty)
                })
                .collect();
            let tail = tail.map(|tail| {
                substitute_named_body_pack(arena, type_subst, pack_subst, memo, pack_memo, tail)
            });
            arena.replace_pack(placeholder, TypePackKind::List { types, tail });
            placeholder
        }
        TypePackKind::Variadic { ty } => {
            let placeholder = arena.alloc_pack(TypePackKind::Variadic { ty });
            pack_memo.insert(id, placeholder);
            let ty = substitute_named_body_type(arena, type_subst, pack_subst, memo, pack_memo, ty);
            arena.replace_pack(placeholder, TypePackKind::Variadic { ty });
            placeholder
        }
    }
}

fn type_generic_name(index: usize) -> String {
    generic_name(index, b'a')
}

fn pack_generic_name(index: usize) -> String {
    generic_name(index, b'A')
}

fn generic_name(index: usize, start: u8) -> String {
    let base = char::from(start + (index % 26) as u8);
    let suffix = index / 26;
    if suffix == 0 {
        base.to_string()
    } else {
        format!("{base}{suffix}")
    }
}

#[cfg(any())]
mod tests;
