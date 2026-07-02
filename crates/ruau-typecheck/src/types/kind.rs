//! Type, table, function, primitive, and type-pack node shapes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Arena, TableAliasIdentity, TypeId, TypeLevel, TypePackId};
use crate::diagnostics::DiagnosticLocation;

/// Canonical handles for primitive, top, bottom, and error types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrimitiveTypes {
    /// `nil`.
    pub nil: TypeId,
    /// `boolean`.
    pub boolean: TypeId,
    /// `number`.
    pub number: TypeId,
    /// `string`.
    pub string: TypeId,
    /// `thread`.
    pub thread: TypeId,
    /// `buffer`.
    pub buffer: TypeId,
    /// `vector`.
    pub vector: TypeId,
    /// Dynamic top type.
    pub any: TypeId,
    /// Unknown type used when inference intentionally withholds a result.
    pub unknown: TypeId,
    /// Bottom type.
    pub never: TypeId,
    /// Error recovery type.
    pub error: TypeId,
}

impl PrimitiveTypes {
    /// Placeholder handles overwritten by [`Arena::new`] during construction.
    pub(crate) fn placeholder() -> Self {
        let placeholder = TypeId::from_index(0);
        Self {
            nil: placeholder,
            boolean: placeholder,
            number: placeholder,
            string: placeholder,
            thread: placeholder,
            buffer: placeholder,
            vector: placeholder,
            any: placeholder,
            unknown: placeholder,
            never: placeholder,
            error: placeholder,
        }
    }
}

/// Type node stored in a [`Arena`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TypeKind {
    /// Built-in primitive scalar or runtime type.
    Primitive(PrimitiveType),
    /// Singleton literal type.
    Singleton(SingletonType),
    /// Function type with argument and return packs.
    Function(FunctionType),
    /// Table or class-like structural type.
    Table(TableType),
    /// Extern/class-like nominal type.
    Extern {
        /// Nominal display name.
        name: String,
        /// Names of known parent extern/class types, nearest parent first.
        parents: Vec<String>,
        /// Readable properties exposed by this extern/class type.
        properties: BTreeMap<String, TableProperty>,
        /// Optional indexer exposed by this extern/class type.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        indexer: Option<TableIndexer>,
    },
    /// Table wrapped in a metatable.
    Metatable {
        /// Table portion.
        table: TypeId,
        /// Metatable portion.
        metatable: TypeId,
        /// Optional nominal display name.
        name: Option<String>,
    },
    /// User-defined type function instance.
    TypeFunctionInstance {
        /// Function display name.
        name: String,
        /// Type arguments.
        arguments: Vec<TypeId>,
    },
    /// Union type. Empty unions are represented as `never` by construction in
    /// later normalization code; the raw arena still permits the shape.
    Union(Vec<TypeId>),
    /// Intersection type.
    Intersection(Vec<TypeId>),
    /// Negated type used by flow-sensitive refinements.
    Negation(TypeId),
    /// Type variable bound to another type.
    Bound(TypeId),
    /// Free type variable waiting for solver constraints.
    Free(TypeVariable),
    /// Type whose constraints are blocked on unresolved data flow.
    Blocked(BlockedType),
    /// Generic type parameter.
    Generic(GenericType),
    /// Error recovery type.
    Error,
    /// Unknown type used for incomplete inference results.
    Unknown,
    /// Bottom type.
    Never,
    /// Dynamic type.
    Any,
}

/// The top-level discriminant of a resolved type node, independent of its
/// payload. This is the structured *internal cause* signal the burndown
/// classifier groups dirty type assertions by — read straight off the arena,
/// not parsed from a rendered string. `TypeFunctionInstance` means an unreduced
/// UDTF; `Free` / `Blocked` / `Generic` are unsolved solver state; `Any` /
/// `Unknown` / `Error` are widening fallbacks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum KindTag {
    /// Primitive type.
    Primitive,
    /// Singleton literal type.
    Singleton,
    /// Function type.
    Function,
    /// Table type.
    Table,
    /// Extern/class-like nominal type.
    Extern,
    /// Metatable-wrapped table type.
    Metatable,
    /// Unreduced user-defined type function or type-alias application.
    TypeFunctionInstance,
    /// Union type.
    Union,
    /// Intersection type.
    Intersection,
    /// Negated type used by flow-sensitive refinements.
    Negation,
    /// Free type variable the solver never bound.
    Free,
    /// Constraints blocked on unresolved data flow.
    Blocked,
    /// Generic type parameter.
    Generic,
    /// Error recovery type.
    Error,
    /// Unknown type used for incomplete inference results.
    Unknown,
    /// Bottom type.
    Never,
    /// Dynamic type.
    Any,
}

impl KindTag {
    /// Stable lowercase string used in classifier reports and JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primitive => "primitive",
            Self::Singleton => "singleton",
            Self::Function => "function",
            Self::Table => "table",
            Self::Extern => "extern",
            Self::Metatable => "metatable",
            Self::TypeFunctionInstance => "type-function-instance",
            Self::Union => "union",
            Self::Intersection => "intersection",
            Self::Negation => "negation",
            Self::Free => "free",
            Self::Blocked => "blocked",
            Self::Generic => "generic",
            Self::Error => "error",
            Self::Unknown => "unknown",
            Self::Never => "never",
            Self::Any => "any",
        }
    }
}

impl TypeKind {
    /// The top-level discriminant of this node as a [`KindTag`]. `Bound`
    /// resolves to the tag of nothing here — callers should [`Arena::follow`]
    /// first; a raw `Bound` reports as `Free` (an unresolved indirection).
    #[must_use]
    pub const fn tag(&self) -> KindTag {
        match self {
            Self::Primitive(_) => KindTag::Primitive,
            Self::Singleton(_) => KindTag::Singleton,
            Self::Function(_) => KindTag::Function,
            Self::Table(_) => KindTag::Table,
            Self::Extern { .. } => KindTag::Extern,
            Self::Metatable { .. } => KindTag::Metatable,
            Self::TypeFunctionInstance { .. } => KindTag::TypeFunctionInstance,
            Self::Union(_) => KindTag::Union,
            Self::Intersection(_) => KindTag::Intersection,
            Self::Negation(_) => KindTag::Negation,
            Self::Bound(_) | Self::Free(_) => KindTag::Free,
            Self::Blocked(_) => KindTag::Blocked,
            Self::Generic(_) => KindTag::Generic,
            Self::Error => KindTag::Error,
            Self::Unknown => KindTag::Unknown,
            Self::Never => KindTag::Never,
            Self::Any => KindTag::Any,
        }
    }
}

/// Returns true when an extern/class type with `name` and known `parents`
/// is a subtype of the extern/class type named `super_name`.
#[must_use]
pub fn extern_is_subtype(name: &str, parents: &[String], super_name: &str) -> bool {
    name == super_name || parents.iter().any(|parent| parent == super_name)
}

/// Built-in primitive scalar and runtime type categories.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimitiveType {
    /// `nil`.
    Nil,
    /// `boolean`.
    Boolean,
    /// `number`.
    Number,
    /// `string`.
    String,
    /// `thread`.
    Thread,
    /// `buffer`.
    Buffer,
    /// `vector`.
    Vector,
}

/// Literal singleton type.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum SingletonType {
    /// `true` or `false`.
    Boolean(bool),
    /// String singleton.
    String(String),
}

impl SingletonType {
    /// The primitive type that this singleton refines.
    #[must_use]
    pub(crate) fn primitive(&self) -> PrimitiveType {
        match self {
            Self::Boolean(_) => PrimitiveType::Boolean,
            Self::String(_) => PrimitiveType::String,
        }
    }
}

/// Function type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FunctionType {
    /// Type parameters owned by this function.
    pub generics: Vec<GenericType>,
    /// Type-pack parameters owned by this function.
    pub generic_packs: Vec<GenericTypePack>,
    /// Optional source names for positional arguments.
    pub argument_names: Vec<Option<String>>,
    /// True when the first argument is an implicit self parameter.
    pub has_self: bool,
    /// True for checked functions.
    pub is_checked: bool,
    /// Argument pack.
    pub arguments: TypePackId,
    /// Return pack.
    pub returns: TypePackId,
}

impl FunctionType {
    /// Creates a plain function type with no type parameters, argument names,
    /// self parameter, or checked marker.
    #[must_use]
    pub fn new(arguments: TypePackId, returns: TypePackId) -> Self {
        Self {
            generics: Vec::new(),
            generic_packs: Vec::new(),
            argument_names: Vec::new(),
            has_self: false,
            is_checked: false,
            arguments,
            returns,
        }
    }
}

/// Table type state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TableState {
    /// Sealed table with a fixed public shape.
    Sealed,
    /// Unsealed table that may acquire new properties during checking.
    #[default]
    Unsealed,
    /// Generic table parameter.
    Generic,
    /// Free table variable.
    Free,
}

/// Structural table type.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TableType {
    /// Optional nominal display name.
    pub name: Option<String>,
    /// Optional source alias definition that owns this nominal table identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_identity: Option<TableAliasIdentity>,
    /// Instantiated type parameters for named table display.
    pub instantiated_type_params: Vec<TypeId>,
    /// Instantiated type-pack parameters for named table display (rendered
    /// after the type parameters, so `Y<T..., U...>` shows its packs).
    pub instantiated_type_pack_params: Vec<TypePackId>,
    /// Named properties sorted by name for deterministic summaries.
    pub properties: BTreeMap<String, TableProperty>,
    /// Optional indexer.
    pub indexer: Option<TableIndexer>,
    /// Current table state.
    pub state: TableState,
}

impl TableType {
    /// Creates an empty table with the supplied state.
    #[must_use]
    pub fn new(state: TableState) -> Self {
        Self {
            name: None,
            alias_identity: None,
            instantiated_type_params: Vec::new(),
            instantiated_type_pack_params: Vec::new(),
            properties: BTreeMap::new(),
            indexer: None,
            state,
        }
    }

    /// Rewrites every property read/write type and the indexer key/value
    /// through `map`, preserving the non-type metadata.
    pub(crate) fn map_value_types(&mut self, mut map: impl FnMut(TypeId) -> TypeId) {
        self.properties = std::mem::take(&mut self.properties)
            .into_iter()
            .map(|(name, mut property)| {
                property.ty = map(property.ty);
                property.write_ty = property.write_ty.map(&mut map);
                (name, property)
            })
            .collect();
        self.indexer = self.indexer.take().map(|mut indexer| {
            indexer.key = map(indexer.key);
            indexer.value = map(indexer.value);
            indexer
        });
    }

    /// Returns true when the table is in the unsealed (growable) state.
    #[must_use]
    pub fn is_unsealed(&self) -> bool {
        matches!(self.state, TableState::Unsealed)
    }

    /// Returns true when the table is in the sealed (closed) state.
    #[must_use]
    #[cfg(any())]
    pub fn is_sealed(&self) -> bool {
        matches!(self.state, TableState::Sealed)
    }

    /// Transitions an unsealed (or free) table into the sealed state.
    pub fn seal(&mut self) {
        self.state = TableState::Sealed;
    }

    /// Merges `other`'s properties and indexer into `self`, adding only fields
    /// that `self` does not already define. Returns true when `self` changed.
    pub fn merge_unsealed_assignment(&mut self, other: Self) -> bool {
        let mut changed = false;
        for (name, property) in other.properties {
            if let std::collections::btree_map::Entry::Vacant(e) = self.properties.entry(name) {
                e.insert(property);
                changed = true;
            }
        }
        if self.indexer.is_none() && other.indexer.is_some() {
            self.indexer = other.indexer;
            changed = true;
        }
        changed
    }
}

/// Returns true when two named tables are the same alias-definition instance.
#[must_use]
pub fn same_named_table_instance(arena: &Arena, left: &TableType, right: &TableType) -> bool {
    left.name.is_some()
        && left.name == right.name
        && left.alias_identity.is_some()
        && left.alias_identity == right.alias_identity
        && left.state == right.state
        && left.instantiated_type_params.len() == right.instantiated_type_params.len()
        && left
            .instantiated_type_params
            .iter()
            .copied()
            .zip(right.instantiated_type_params.iter().copied())
            .all(|(left, right)| arena.follow(left) == arena.follow(right))
        && left.instantiated_type_pack_params.len() == right.instantiated_type_pack_params.len()
        && left
            .instantiated_type_pack_params
            .iter()
            .copied()
            .zip(right.instantiated_type_pack_params.iter().copied())
            .all(|(left, right)| arena.follow_pack(left) == arena.follow_pack(right))
}

/// Returns true when two tables came from the same alias definition with the
/// same table state and the same number of instantiated type and pack arguments.
#[must_use]
pub fn same_alias_identity_table_arity(left: &TableType, right: &TableType) -> bool {
    left.alias_identity.is_some()
        && left.alias_identity == right.alias_identity
        && left.state == right.state
        && left.instantiated_type_params.len() == right.instantiated_type_params.len()
        && left.instantiated_type_pack_params.len() == right.instantiated_type_pack_params.len()
}

/// Returns true when two tables came from the same alias definition and their
/// instantiated type and pack arguments are already equal after following.
#[must_use]
pub fn same_alias_identity_table_instance(
    arena: &Arena,
    left: &TableType,
    right: &TableType,
) -> bool {
    same_alias_identity_table_arity(left, right)
        && left
            .instantiated_type_params
            .iter()
            .copied()
            .zip(right.instantiated_type_params.iter().copied())
            .all(|(left, right)| arena.follow(left) == arena.follow(right))
        && left
            .instantiated_type_pack_params
            .iter()
            .copied()
            .zip(right.instantiated_type_pack_params.iter().copied())
            .all(|(left, right)| arena.follow_pack(left) == arena.follow_pack(right))
}

/// Named table property.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TableProperty {
    /// Property type.
    pub ty: TypeId,
    /// Property type accepted by writes when it differs from the read type.
    #[serde(default)]
    pub write_ty: Option<TypeId>,
    /// Source range for the property name, when it came from source.
    pub location: Option<DiagnosticLocation>,
    /// Documentation symbol for query consumers.
    #[serde(default)]
    pub documentation_symbol: Option<String>,
    /// Whether callers can write the property.
    pub read_only: bool,
    /// Whether callers can read the property.
    pub write_only: bool,
    /// Whether reads should report a deprecation diagnostic.
    pub deprecated: bool,
}

impl TableProperty {
    /// Creates a writable property.
    #[must_use]
    pub const fn new(ty: TypeId) -> Self {
        Self {
            ty,
            write_ty: None,
            location: None,
            documentation_symbol: None,
            read_only: false,
            write_only: false,
            deprecated: false,
        }
    }

    /// Creates a read-only property.
    #[must_use]
    pub const fn read_only(ty: TypeId) -> Self {
        let mut property = Self::new(ty);
        property.read_only = true;
        property
    }

    /// Attaches a source range to the property.
    #[must_use]
    pub const fn with_location(mut self, location: Option<DiagnosticLocation>) -> Self {
        self.location = location;
        self
    }

    /// Attaches a documentation symbol to the property.
    #[must_use]
    pub fn with_documentation_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.documentation_symbol = Some(symbol.into());
        self
    }

    /// Type accepted by writes to this property.
    #[must_use]
    pub fn write_type(&self) -> TypeId {
        self.write_ty.unwrap_or(self.ty)
    }
}

/// Table indexer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TableIndexer {
    /// Key type.
    pub key: TypeId,
    /// Value type.
    pub value: TypeId,
    /// Whether callers can write through this indexer.
    pub read_only: bool,
}

/// Free type variable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TypeVariable {
    /// Solver level.
    pub level: TypeLevel,
    /// Optional display name.
    pub name: Option<String>,
    /// Optional lower bound.
    pub lower_bound: Option<TypeId>,
    /// Optional upper bound.
    pub upper_bound: Option<TypeId>,
}

/// Blocked type placeholder.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlockedType {
    /// Optional stable blocker label for diagnostics and snapshots.
    pub reason: Option<String>,
}

/// Generic type parameter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GenericType {
    /// Parameter name.
    pub name: String,
    /// Level at which the parameter was generalized.
    pub level: TypeLevel,
}

/// Generic type-pack parameter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GenericTypePack {
    /// Parameter name.
    pub name: String,
    /// Level at which the parameter was generalized.
    pub level: TypeLevel,
}

/// Type-pack node stored in a [`Arena`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TypePackKind {
    /// Fixed prefix with an optional tail pack.
    List {
        /// Fixed type prefix.
        types: Vec<TypeId>,
        /// Optional tail pack.
        tail: Option<TypePackId>,
    },
    /// Variadic repetition of one type.
    Variadic {
        /// Repeated type.
        ty: TypeId,
    },
    /// Free type-pack variable.
    Free {
        /// Solver level.
        level: TypeLevel,
        /// Optional display name.
        name: Option<String>,
    },
    /// Generic type-pack parameter.
    Generic(GenericTypePack),
    /// Pack variable bound to another pack.
    Bound(TypePackId),
    /// Error recovery pack.
    Error,
}

/// Normalized view of a type pack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedTypePack {
    /// Flattened concrete type prefix.
    pub types: Vec<TypeId>,
    /// Remaining non-list tail, if any.
    pub tail: Option<TypePackTail>,
}

/// Tail remaining after pack normalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypePackTail {
    /// Free type-pack variable.
    Free {
        /// Solver level.
        level: TypeLevel,
        /// Optional display name.
        name: Option<String>,
    },
    /// Generic type-pack parameter.
    Generic(GenericTypePack),
    /// Variadic repetition.
    Variadic(TypeId),
    /// Error recovery tail.
    Error,
    /// Cyclic pack tail.
    Cycle(TypePackId),
}

/// Flattened list-pack view that preserves the original pack id for
/// diagnostics and keeps the remaining bindable tail as a pack handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlattenedListPack {
    /// Original followed list-pack id.
    pub(crate) id: TypePackId,
    /// Flattened fixed prefix.
    pub(crate) types: Vec<TypeId>,
    /// Remaining non-list tail, if any.
    pub(crate) tail: Option<TypePackId>,
}

impl FlattenedListPack {
    /// Splits the first fixed type from this list view.
    #[must_use]
    pub(crate) fn split_first(mut self) -> Option<(TypeId, Self)> {
        if self.types.is_empty() {
            return None;
        }
        let first = self.types.remove(0);
        Some((first, self))
    }
}

/// Allocates Luau's top function type, rendered as `function`.
pub fn alloc_top_function_type(arena: &mut Arena) -> TypeId {
    let any = arena.primitives().any;
    let arguments = arena.alloc_pack(TypePackKind::Variadic { ty: any });
    let returns = arena.alloc_pack(TypePackKind::Variadic { ty: any });
    arena.alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
}

/// Returns true when a function type is Luau's top function type.
pub fn is_top_function_type(arena: &Arena, function: &FunctionType) -> bool {
    function.generics.is_empty()
        && function.generic_packs.is_empty()
        && function.argument_names.is_empty()
        && !function.has_self
        && !function.is_checked
        && is_variadic_any_pack(arena, function.arguments)
        && is_variadic_any_pack(arena, function.returns)
}

fn is_variadic_any_pack(arena: &Arena, pack: TypePackId) -> bool {
    matches!(
        arena.get_pack(pack),
        TypePackKind::Variadic { ty } if matches!(arena.get(*ty), TypeKind::Any)
    )
}

impl PrimitiveType {
    /// Luau spelling for this primitive.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Nil => "nil",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::String => "string",
            Self::Thread => "thread",
            Self::Buffer => "buffer",
            Self::Vector => "vector",
        }
    }
}
