//! Type arena allocation, primitive handles, and graph-wide structural helpers.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    BlockedType, FlattenedListPack, FunctionType, GenericType, NormalizedTypePack, PrimitiveType,
    PrimitiveTypes, SingletonType, TableIndexer, TableProperty, TableState, TableType, TypeId,
    TypeKind, TypePackId, TypePackKind, TypePackTail, TypeVariable,
};

/// Arena-owned type graph.
///
/// Handles are stable for the lifetime of the arena. Cross-arena sharing is
/// intentionally explicit: later module-checking work can either share a
/// session arena or add a translation layer without changing the handle shape.
#[derive(Clone, Debug)]
pub struct Arena {
    /// Allocated types.
    pub(crate) types: Vec<TypeKind>,
    /// Allocated type packs.
    pub(crate) packs: Vec<TypePackKind>,
    /// Canonical built-in handles allocated by [`Arena::new`].
    primitives: PrimitiveTypes,
    /// Canonical empty pack allocated by [`Arena::new`].
    empty_pack: TypePackId,
}

/// Allocation checkpoint for speculative type-arena probes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArenaCheckpoint {
    type_len: usize,
    pack_len: usize,
}

/// Borrowed type node after following `Bound` links.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub enum FollowedTypeKind<'a> {
    Primitive(&'a PrimitiveType),
    Singleton(&'a SingletonType),
    Function(&'a FunctionType),
    Table(&'a TableType),
    Extern {
        name: &'a str,
        parents: &'a [String],
        properties: &'a BTreeMap<String, TableProperty>,
        indexer: Option<&'a TableIndexer>,
    },
    Metatable {
        table: TypeId,
        metatable: TypeId,
        name: Option<&'a str>,
    },
    TypeFunctionInstance {
        name: &'a str,
        arguments: &'a [TypeId],
    },
    Union(&'a [TypeId]),
    Intersection(&'a [TypeId]),
    Negation(TypeId),
    Free(&'a TypeVariable),
    Blocked(&'a BlockedType),
    Generic(&'a GenericType),
    Error,
    Unknown,
    Never,
    Any,
}

impl<'a> From<&'a TypeKind> for FollowedTypeKind<'a> {
    fn from(kind: &'a TypeKind) -> Self {
        match kind {
            TypeKind::Primitive(primitive) => Self::Primitive(primitive),
            TypeKind::Singleton(singleton) => Self::Singleton(singleton),
            TypeKind::Function(function) => Self::Function(function),
            TypeKind::Table(table) => Self::Table(table),
            TypeKind::Extern {
                name,
                parents,
                properties,
                indexer,
            } => Self::Extern {
                name,
                parents,
                properties,
                indexer: indexer.as_ref(),
            },
            TypeKind::Metatable {
                table,
                metatable,
                name,
            } => Self::Metatable {
                table: *table,
                metatable: *metatable,
                name: name.as_deref(),
            },
            TypeKind::TypeFunctionInstance { name, arguments } => {
                Self::TypeFunctionInstance { name, arguments }
            }
            TypeKind::Union(types) => Self::Union(types),
            TypeKind::Intersection(types) => Self::Intersection(types),
            TypeKind::Negation(inner) => Self::Negation(*inner),
            TypeKind::Bound(_) => unreachable!("followed type views cannot contain bound types"),
            TypeKind::Free(free) => Self::Free(free),
            TypeKind::Blocked(blocked) => Self::Blocked(blocked),
            TypeKind::Generic(generic) => Self::Generic(generic),
            TypeKind::Error => Self::Error,
            TypeKind::Unknown => Self::Unknown,
            TypeKind::Never => Self::Never,
            TypeKind::Any => Self::Any,
        }
    }
}

impl Arena {
    /// Creates an arena with canonical primitive, top, bottom, and empty-pack
    /// handles allocated first.
    #[must_use]
    pub fn new() -> Self {
        let mut arena = Self {
            types: Vec::new(),
            packs: Vec::new(),
            primitives: PrimitiveTypes::default(),
            empty_pack: TypePackId::from_index(0),
        };
        arena.primitives.nil = arena.alloc(TypeKind::Primitive(PrimitiveType::Nil));
        arena.primitives.boolean = arena.alloc(TypeKind::Primitive(PrimitiveType::Boolean));
        arena.primitives.number = arena.alloc(TypeKind::Primitive(PrimitiveType::Number));
        arena.primitives.string = arena.alloc(TypeKind::Primitive(PrimitiveType::String));
        arena.primitives.thread = arena.alloc(TypeKind::Primitive(PrimitiveType::Thread));
        arena.primitives.buffer = arena.alloc(TypeKind::Primitive(PrimitiveType::Buffer));
        arena.primitives.vector = arena.alloc(TypeKind::Primitive(PrimitiveType::Vector));
        arena.primitives.any = arena.alloc(TypeKind::Any);
        arena.primitives.unknown = arena.alloc(TypeKind::Unknown);
        arena.primitives.never = arena.alloc(TypeKind::Never);
        arena.primitives.error = arena.alloc(TypeKind::Error);
        arena.empty_pack = arena.alloc_pack(TypePackKind::List {
            types: Vec::new(),
            tail: None,
        });
        arena
    }

    /// Allocates a type and returns its stable handle.
    pub(crate) fn alloc(&mut self, kind: TypeKind) -> TypeId {
        let id = TypeId::from_index(self.types.len());
        self.types.push(kind);
        id
    }

    /// Allocates a type pack and returns its stable handle.
    pub(crate) fn alloc_pack(&mut self, kind: TypePackKind) -> TypePackId {
        let id = TypePackId::from_index(self.packs.len());
        self.packs.push(kind);
        id
    }

    /// Captures the current allocation frontier for a speculative probe.
    #[must_use]
    pub(crate) fn checkpoint(&self) -> ArenaCheckpoint {
        ArenaCheckpoint {
            type_len: self.types.len(),
            pack_len: self.packs.len(),
        }
    }

    /// Rolls back allocations made after `checkpoint`.
    ///
    /// This is intentionally allocation-only: callers must not use it to undo
    /// `replace` or `replace_pack` mutations.
    pub(crate) fn rollback_to(&mut self, checkpoint: ArenaCheckpoint) {
        self.types.truncate(checkpoint.type_len);
        self.packs.truncate(checkpoint.pack_len);
    }

    /// Returns an allocated type.
    #[must_use]
    pub(crate) fn get(&self, id: TypeId) -> &TypeKind {
        &self.types[id.index()]
    }

    /// Returns the stable handle at a zero-based arena index.
    ///
    /// This is crate-visible for arena-wide maintenance passes that need to
    /// inspect or replace many existing nodes without exposing raw handle
    /// construction publicly.
    #[must_use]
    pub(crate) fn type_id_at(&self, index: usize) -> TypeId {
        TypeId::from_index(index)
    }

    /// Returns an allocated type pack.
    #[must_use]
    pub(crate) fn get_pack(&self, id: TypePackId) -> &TypePackKind {
        &self.packs[id.index()]
    }

    /// Walks a `TypeKind::Bound` chain to its resolved handle.
    ///
    /// Bound chains are short in practice; a small `Vec` beats `BTreeSet`
    /// allocation for cycle detection.
    #[must_use]
    pub fn follow(&self, mut id: TypeId) -> TypeId {
        let mut seen = Vec::new();
        loop {
            if seen.contains(&id) {
                return id;
            }
            seen.push(id);
            match self.get(id) {
                TypeKind::Bound(next) => id = *next,
                _ => return id,
            }
        }
    }

    /// Follows `Bound` links and returns the resolved id with a non-bound view.
    #[must_use]
    pub(crate) fn followed(&self, id: TypeId) -> (TypeId, FollowedTypeKind<'_>) {
        let id = self.follow(id);
        (id, FollowedTypeKind::from(self.get(id)))
    }

    /// Walks a `TypePackKind::Bound` chain to its resolved handle.
    #[must_use]
    pub(crate) fn follow_pack(&self, mut id: TypePackId) -> TypePackId {
        let mut seen = Vec::new();
        loop {
            if seen.contains(&id) {
                return id;
            }
            seen.push(id);
            match self.get_pack(id) {
                TypePackKind::Bound(next) => id = *next,
                _ => return id,
            }
        }
    }

    /// Replaces an allocated type node while preserving the stable handle.
    pub(crate) fn replace(&mut self, id: TypeId, kind: TypeKind) {
        self.types[id.index()] = kind;
    }

    /// Replaces an allocated type-pack node while preserving the stable handle.
    pub(crate) fn replace_pack(&mut self, id: TypePackId, kind: TypePackKind) {
        self.packs[id.index()] = kind;
    }

    /// Canonical primitive and lattice handles for this arena.
    #[must_use]
    pub(crate) const fn primitives(&self) -> PrimitiveTypes {
        self.primitives
    }

    /// Canonical empty pack for this arena.
    #[must_use]
    pub(crate) const fn empty_pack(&self) -> TypePackId {
        self.empty_pack
    }

    /// Number of allocated types.
    #[must_use]
    pub fn type_len(&self) -> usize {
        self.types.len()
    }

    /// Number of allocated type packs.
    #[must_use]
    pub fn pack_len(&self) -> usize {
        self.packs.len()
    }

    /// Seals every unsealed table in the arena. Run after solving so callers
    /// observe a stable table shape.
    pub(crate) fn finalize_unsealed_tables(&mut self) {
        for index in 0..self.type_len() {
            let id = self.type_id_at(index);
            let TypeKind::Table(mut table) = self.get(id).clone() else {
                continue;
            };
            if table.is_unsealed() {
                table.seal();
                self.replace(id, TypeKind::Table(table));
            }
        }
    }

    /// Returns the first concrete type in a pack after chasing list tails and
    /// bound packs.
    #[must_use]
    pub(crate) fn first_in_pack(&self, id: TypePackId) -> Option<TypeId> {
        self.normalize_pack(id).types.first().copied()
    }

    /// Flattens list-pack prefixes and follows bound packs.
    ///
    /// Normalization stops at free, generic, variadic, error, missing, or
    /// cyclic tails. This mirrors the part of upstream `TypePackIterator`
    /// needed before the constraint solver starts mutating pack variables.
    #[must_use]
    pub(crate) fn normalize_pack(&self, id: TypePackId) -> NormalizedTypePack {
        let mut normalizer = PackNormalizer {
            arena: self,
            seen: Vec::new(),
            types: Vec::new(),
        };
        normalizer.normalize(id)
    }

    /// Splices list-pack tails that themselves resolve to list packs into the
    /// fixed prefix, preserving the remaining tail's bindable pack id.
    ///
    /// Substituting a concrete pack for a generic pack reference (`(T...)`
    /// with `T... = (number)`) or binding a free tail to a list produces
    /// `List { types, tail: List { .. } }`; comparators that count arity by
    /// fixed-prefix length must see the spliced shape. Stops at free,
    /// generic, variadic, error, or cyclic tails, which keep their pack id so
    /// callers can still bind them.
    #[must_use]
    pub(crate) fn flatten_list_pack_parts(
        &self,
        mut types: Vec<TypeId>,
        mut tail: Option<TypePackId>,
    ) -> (Vec<TypeId>, Option<TypePackId>) {
        let mut seen = Vec::new();
        while let Some(current) = tail {
            let followed = self.follow_pack(current);
            if seen.contains(&followed) {
                return (types, Some(followed));
            }
            seen.push(followed);
            match self.get_pack(followed) {
                TypePackKind::List {
                    types: more,
                    tail: next,
                } => {
                    types.extend(more.iter().copied());
                    tail = *next;
                }
                _ => return (types, Some(followed)),
            }
        }
        (types, None)
    }

    /// Returns a flattened list-pack view, preserving the original followed
    /// list id for diagnostics and the remaining bindable tail as a pack id.
    #[must_use]
    pub(crate) fn flatten_list_pack(&self, id: TypePackId) -> Option<FlattenedListPack> {
        let id = self.follow_pack(id);
        let TypePackKind::List { types, tail } = self.get_pack(id).clone() else {
            return None;
        };
        Some(self.flatten_list_pack_from_parts(id, types, tail))
    }

    /// Builds a flattened list-pack view from an already matched list payload.
    #[must_use]
    pub(crate) fn flatten_list_pack_from_parts(
        &self,
        id: TypePackId,
        types: Vec<TypeId>,
        tail: Option<TypePackId>,
    ) -> FlattenedListPack {
        let (types, tail) = self.flatten_list_pack_parts(types, tail);
        FlattenedListPack { id, types, tail }
    }

    /// Splits the first fixed type from a flattened list-pack view.
    #[must_use]
    pub(crate) fn split_first_in_list_pack(
        &self,
        id: TypePackId,
    ) -> Option<(TypeId, FlattenedListPack)> {
        self.flatten_list_pack(id)?.split_first()
    }

    /// Returns fixed types for finite packs after following and flattening list
    /// tails. Any non-list tail means the pack is not finite.
    #[must_use]
    pub(crate) fn finite_pack_types(&self, id: TypePackId) -> Option<Vec<TypeId>> {
        let normalized = self.normalize_pack(id);
        normalized.tail.is_none().then_some(normalized.types)
    }

    /// Allocates a pack node equivalent to a normalized tail.
    pub(crate) fn alloc_pack_tail(&mut self, tail: TypePackTail) -> TypePackId {
        match tail {
            TypePackTail::Variadic(ty) => self.alloc_pack(TypePackKind::Variadic { ty }),
            TypePackTail::Free { level, name } => {
                self.alloc_pack(TypePackKind::Free { level, name })
            }
            TypePackTail::Generic(pack) => self.alloc_pack(TypePackKind::Generic(pack)),
            TypePackTail::Error => self.alloc_pack(TypePackKind::Error),
            TypePackTail::Cycle(id) => id,
        }
    }

    /// Allocates a pack node for a normalized tail when present.
    pub(crate) fn alloc_optional_pack_tail(
        &mut self,
        tail: Option<TypePackTail>,
    ) -> Option<TypePackId> {
        tail.map(|tail| self.alloc_pack_tail(tail))
    }

    /// Returns the flattened, iteration-order-preserving options of a union.
    ///
    /// Nested unions are expanded depth-first and cyclic union references are
    /// skipped. Non-union inputs are treated as a one-element sequence, matching
    /// the representation helper behavior callers need before subtyping exists.
    #[must_use]
    pub(crate) fn union_options(&self, id: TypeId) -> Vec<TypeId> {
        let mut flattener = UnionFlattener {
            arena: self,
            active: Vec::new(),
            options: Vec::new(),
        };
        flattener.visit(id);
        flattener.options
    }

    /// Returns true when every flattened option in `sub` appears in `sup`.
    #[must_use]
    #[cfg(any())]
    pub fn union_contains_all(&self, sup: TypeId, sub: TypeId) -> bool {
        let sup_options = self.union_options(sup);
        self.union_options(sub)
            .iter()
            .all(|candidate| sup_options.contains(candidate))
    }

    /// Returns true when `id` is a string primitive, string singleton, or union
    /// containing only string primitives and string singletons.
    #[must_use]
    pub(crate) fn is_string_like(&self, id: TypeId) -> bool {
        self.flattened_options_all(id, |kind| {
            matches!(
                kind,
                TypeKind::Primitive(PrimitiveType::String)
                    | TypeKind::Singleton(SingletonType::String(_))
            )
        })
    }

    /// Returns true when `id` is a boolean primitive, boolean singleton, or
    /// union containing only boolean primitives and boolean singletons.
    #[must_use]
    #[cfg(any())]
    pub(crate) fn is_boolean_like(&self, id: TypeId) -> bool {
        self.flattened_options_all(id, |kind| {
            matches!(
                kind,
                TypeKind::Primitive(PrimitiveType::Boolean)
                    | TypeKind::Singleton(SingletonType::Boolean(_))
            )
        })
    }

    /// Returns the metatable attached to `id` when `id` is a metatable type,
    /// following indirections through both the outer type and the metatable.
    #[must_use]
    pub(crate) fn metatable_payload(&self, id: TypeId) -> Option<TypeId> {
        match self.get(self.follow(id)) {
            TypeKind::Metatable { metatable, .. } => Some(self.follow(*metatable)),
            _ => None,
        }
    }

    /// Returns true when `id` is exactly the `nil` primitive (after following).
    #[must_use]
    pub(crate) fn is_nil(&self, id: TypeId) -> bool {
        matches!(
            self.get(self.follow(id)),
            TypeKind::Primitive(PrimitiveType::Nil)
        )
    }

    /// Returns true when any flattened union member of `id` is `nil`.
    #[must_use]
    pub(crate) fn may_be_nil(&self, id: TypeId) -> bool {
        self.union_options(self.follow(id))
            .into_iter()
            .any(|option| self.is_nil(option))
    }

    /// Returns true when `id` is an optional type: it can be `nil` but is not
    /// just `nil`.
    #[must_use]
    pub(crate) fn is_optional(&self, id: TypeId) -> bool {
        self.may_be_nil(id) && !self.is_nil(id)
    }

    /// Returns true when an unsealed-table indexer key should be widened out
    /// of its local free-variable scope before becoming part of the table
    /// surface.
    #[must_use]
    pub(crate) fn unsealed_indexer_key_needs_unknown_scope(&self, key: TypeId) -> bool {
        matches!(
            self.get(self.follow(key)),
            TypeKind::Free(variable)
                if variable.lower_bound.is_none() && variable.upper_bound.is_none()
        )
    }

    /// Returns the stored key type for a newly inferred unsealed-table indexer.
    #[must_use]
    pub(crate) fn scoped_unsealed_indexer_key(&self, key: TypeId) -> TypeId {
        if self.unsealed_indexer_key_needs_unknown_scope(key) {
            self.primitives().unknown
        } else {
            self.follow(key)
        }
    }

    /// Returns true when reading through an inferred unsealed-table indexer may
    /// produce no value for the requested key.
    #[must_use]
    pub(crate) fn unsealed_indexer_read_may_be_absent(
        &self,
        state: TableState,
        key: TypeId,
    ) -> bool {
        matches!(state, TableState::Unsealed | TableState::Free)
            && matches!(self.get(self.follow(key)), TypeKind::Unknown)
    }

    /// Reads a property directly from a table's own fields, following only the
    /// table payload of metatable wrappers.
    #[must_use]
    pub(crate) fn direct_read_property(&self, table: TypeId, property: &str) -> Option<TypeId> {
        self.direct_read_property_with_seen(table, property, &mut BTreeSet::new())
    }

    fn direct_read_property_with_seen(
        &self,
        table: TypeId,
        property: &str,
        seen: &mut BTreeSet<TypeId>,
    ) -> Option<TypeId> {
        let table = self.follow(table);
        if !seen.insert(table) {
            return None;
        }
        match self.get(table) {
            TypeKind::Table(table_type) => {
                table_type
                    .properties
                    .get(property)
                    .and_then(|table_property| {
                        if table_property.write_only
                            && !matches!(table_type.state, TableState::Unsealed | TableState::Free)
                        {
                            None
                        } else {
                            Some(table_property.ty)
                        }
                    })
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.direct_read_property_with_seen(*base_table, property, seen),
            _ => None,
        }
    }

    /// Checks a predicate over the flattened union view of a type.
    fn flattened_options_all(&self, id: TypeId, predicate: impl Fn(&TypeKind) -> bool) -> bool {
        let options = self.union_options(id);
        !options.is_empty() && options.iter().all(|option| predicate(self.get(*option)))
    }

    /// Copies a type graph, replacing free types with `any` and free packs with
    /// an `...any` pack while preserving recursive structural edges.
    #[cfg(any())]
    pub(crate) fn anyify_type_graph(&mut self, id: TypeId) -> TypeId {
        crate::type_graph::anyify_type_graph(self, id)
    }

    /// Copies a type graph for a module public surface, replacing unresolved
    /// free types and packs with errors while preserving recursive structural
    /// edges.
    pub(crate) fn publicize_type_graph(&mut self, id: TypeId) -> TypeId {
        crate::type_graph::publicize_type_graph(self, id)
    }
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

/// Type-pack normalization worker.
struct PackNormalizer<'arena> {
    /// Type arena being normalized.
    arena: &'arena Arena,
    /// Visited pack ids.
    seen: Vec<TypePackId>,
    /// Flattened type prefix.
    types: Vec<TypeId>,
}

impl PackNormalizer<'_> {
    /// Normalizes a pack id.
    fn normalize(&mut self, id: TypePackId) -> NormalizedTypePack {
        let tail = self.visit(id);
        NormalizedTypePack {
            types: std::mem::take(&mut self.types),
            tail,
        }
    }

    /// Visits one pack node and returns the remaining tail.
    fn visit(&mut self, id: TypePackId) -> Option<TypePackTail> {
        if self.seen.contains(&id) {
            return Some(TypePackTail::Cycle(id));
        }
        self.seen.push(id);

        let tail = match self.arena.get_pack(id) {
            TypePackKind::List { types, tail } => {
                self.types.extend(types.iter().copied());
                tail.and_then(|tail| self.visit(tail))
            }
            TypePackKind::Variadic { ty } => Some(TypePackTail::Variadic(*ty)),
            TypePackKind::Free { level, name } => Some(TypePackTail::Free {
                level: *level,
                name: name.clone(),
            }),
            TypePackKind::Generic(pack) => Some(TypePackTail::Generic(pack.clone())),
            TypePackKind::Bound(bound) => self.visit(*bound),
            TypePackKind::Error => Some(TypePackTail::Error),
        };

        self.seen.pop();
        tail
    }
}

/// Union flattening worker.
struct UnionFlattener<'arena> {
    /// Type arena being traversed.
    arena: &'arena Arena,
    /// Active union ids, used to avoid cycles.
    active: Vec<TypeId>,
    /// Flattened options.
    options: Vec<TypeId>,
}

impl UnionFlattener<'_> {
    /// Visits one type, flattening nested unions.
    fn visit(&mut self, id: TypeId) {
        let TypeKind::Union(options) = self.arena.get(id) else {
            self.options.push(id);
            return;
        };

        if self.active.contains(&id) {
            return;
        }

        self.active.push(id);
        for option in options {
            self.visit(*option);
        }
        self.active.pop();
    }
}
