//! Shared member-access facts over type shapes.
//!
//! Solvers still own mutation, constraint emission, and diagnostic aggregation.
//! This module names the pure policy used by those solvers: whether a property
//! may be read or written, how a member key relates to an indexer key, and which
//! properties are inherited through table-valued `__index` metatables.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    builtins::{string_primitive_property_type, vector_primitive_property_type},
    types::{
        Arena, FunctionType, PrimitiveType, SingletonType, TableIndexer, TableProperty, TableState,
        TableType, TypeId, TypeKind,
    },
};

/// Member key normalized at a use site.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemberKey {
    /// Named property access, including string-singleton index expressions.
    Property(String),
    /// Non-property index access.
    Index(TypeId),
}

impl MemberKey {
    /// Builds an index key, normalizing string singletons to property keys.
    pub fn from_index(arena: &Arena, key: TypeId) -> Self {
        let key = arena.follow(key);
        match arena.get(key) {
            TypeKind::Singleton(SingletonType::String(value)) => Self::Property(value.clone()),
            _ => Self::Index(key),
        }
    }
}

/// Structural member result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemberResolution {
    /// A named property was selected.
    Property(TableProperty),
    /// A table indexer was selected.
    Indexer(TableIndexer),
}

impl MemberResolution {
    /// Type produced by a read.
    pub fn read_type(&self) -> TypeId {
        match self {
            Self::Property(property) => property.ty,
            Self::Indexer(indexer) => indexer.value,
        }
    }
}

/// Reason a pure member lookup cannot produce a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberFailure {
    /// The member is absent.
    Missing,
    /// The index key is incompatible with an indexer key.
    KeyMismatch,
    /// The base shape is not handled by the pure helper.
    Unsupported,
}

/// Returns true for table states whose member surface may still be extended or
/// whose read-only/write-only markers are relaxed while inference is open.
pub fn table_state_allows_member_extension(state: TableState) -> bool {
    matches!(state, TableState::Unsealed | TableState::Free)
}

/// Returns true when `property` may be read from a table with `state`.
pub fn table_property_allows_read(property: &TableProperty, state: TableState) -> bool {
    !property.write_only || table_state_allows_member_extension(state)
}

/// Returns true when `property` may be written on a table with `state`.
pub fn table_property_allows_write(property: &TableProperty, state: TableState) -> bool {
    !property.read_only || table_state_allows_member_extension(state)
}

/// Returns true when writing an open/free read-only property should promote the
/// stored table member to a writable property.
pub fn table_property_promotes_on_write(property: &TableProperty, state: TableState) -> bool {
    property.read_only && !property.write_only && table_state_allows_member_extension(state)
}

/// Extern/class properties are closed; write-only means unreadable.
pub fn extern_property_allows_read(property: &TableProperty) -> bool {
    !property.write_only
}

/// Extern/class properties are closed; read-only means unwritable.
pub fn extern_property_allows_write(property: &TableProperty) -> bool {
    !property.read_only
}

/// Returns the string singleton name for an index key, if present.
pub fn string_singleton_key(arena: &Arena, key: TypeId) -> Option<String> {
    match MemberKey::from_index(arena, key) {
        MemberKey::Property(name) => Some(name),
        MemberKey::Index(_) => None,
    }
}

/// Allocates the singleton string type used to compare a property name against
/// an indexer key.
pub fn property_name_key(arena: &mut Arena, name: &str) -> TypeId {
    arena.alloc(TypeKind::Singleton(SingletonType::String(name.to_owned())))
}

/// Returns true when a property name is accepted by an indexer key type.
pub fn property_name_matches_key(arena: &Arena, name: &str, key: TypeId) -> bool {
    match arena.get(arena.follow(key)) {
        TypeKind::Primitive(PrimitiveType::String)
        | TypeKind::Any
        | TypeKind::Unknown
        | TypeKind::Error
        | TypeKind::Blocked(_)
        | TypeKind::Free(_)
        | TypeKind::Generic(_) => true,
        TypeKind::Singleton(SingletonType::String(value)) => value == name,
        TypeKind::Union(options) => options
            .iter()
            .any(|option| property_name_matches_key(arena, name, *option)),
        TypeKind::Intersection(options) => options
            .iter()
            .all(|option| property_name_matches_key(arena, name, *option)),
        TypeKind::Negation(inner) => !property_name_matches_key(arena, name, *inner),
        _ => false,
    }
}

/// Returns true when `key` can read through `indexer` under the reduced
/// type-function rules.
pub fn indexer_accepts_key(arena: &Arena, indexer: &TableIndexer, key: TypeId) -> bool {
    type_is_subtype_of_key(arena, key, indexer.key)
}

fn type_is_subtype_of_key(arena: &Arena, sub: TypeId, sup: TypeId) -> bool {
    let sub = arena.follow(sub);
    let sup = arena.follow(sup);
    if sub == sup {
        return true;
    }

    match (arena.get(sub), arena.get(sup)) {
        (TypeKind::Union(options), _) => options
            .iter()
            .all(|option| type_is_subtype_of_key(arena, *option, sup)),
        (_, TypeKind::Union(options)) => options
            .iter()
            .any(|option| type_is_subtype_of_key(arena, sub, *option)),
        (
            TypeKind::Singleton(SingletonType::String(_)),
            TypeKind::Primitive(PrimitiveType::String),
        )
        | (
            TypeKind::Singleton(SingletonType::Boolean(_)),
            TypeKind::Primitive(PrimitiveType::Boolean),
        ) => true,
        _ => false,
    }
}

/// Resolves a direct read from a concrete table under type-function semantics.
pub fn table_index_member(
    arena: &Arena,
    table: &TableType,
    key: TypeId,
) -> Result<MemberResolution, MemberFailure> {
    if let MemberKey::Property(name) = MemberKey::from_index(arena, key)
        && let Some(property) = table.properties.get(&name)
    {
        return Ok(MemberResolution::Property(property.clone()));
    }

    if let Some(indexer) = &table.indexer {
        if indexer_accepts_key(arena, indexer, key) {
            return Ok(MemberResolution::Indexer(indexer.clone()));
        }
        return if index_key_is_definitely_absent(arena, key) {
            Err(MemberFailure::Missing)
        } else {
            Err(MemberFailure::KeyMismatch)
        };
    }

    if index_key_is_definitely_absent(arena, key) {
        Err(MemberFailure::Missing)
    } else {
        Err(MemberFailure::Unsupported)
    }
}

fn index_key_is_definitely_absent(arena: &Arena, key: TypeId) -> bool {
    match arena.get(arena.follow(key)) {
        TypeKind::Singleton(_) => true,
        TypeKind::Primitive(primitive) => match primitive {
            PrimitiveType::Nil
            | PrimitiveType::Boolean
            | PrimitiveType::Number
            | PrimitiveType::String
            | PrimitiveType::Thread
            | PrimitiveType::Buffer
            | PrimitiveType::Vector => true,
        },
        TypeKind::Bound(_) => unreachable!("follow removes bound types"),
        TypeKind::Function(_)
        | TypeKind::Table(_)
        | TypeKind::Extern { .. }
        | TypeKind::Metatable { .. }
        | TypeKind::TypeFunctionInstance { .. }
        | TypeKind::Union(_)
        | TypeKind::Intersection(_)
        | TypeKind::Negation(_)
        | TypeKind::Free(_)
        | TypeKind::Blocked(_)
        | TypeKind::Generic(_)
        | TypeKind::Error
        | TypeKind::Unknown
        | TypeKind::Never
        | TypeKind::Any => false,
    }
}

/// Returns properties inherited through a table-valued `__index` metatable.
/// Inherited properties are read-only because writes still target the base
/// table.
pub fn metatable_index_table_properties(
    arena: &Arena,
    metatable: TypeId,
) -> Option<BTreeMap<String, TableProperty>> {
    let TypeKind::Table(metatable) = arena.get(arena.follow(metatable)) else {
        return None;
    };
    let index = metatable.properties.get("__index")?.ty;
    let TypeKind::Table(index_table) = arena.get(arena.follow(index)) else {
        return None;
    };
    Some(
        index_table
            .properties
            .iter()
            .map(|(name, property)| {
                let mut property = property.clone();
                property.read_only = true;
                property.write_only = false;
                (name.clone(), property)
            })
            .collect(),
    )
}

/// Returns a read-only indexer synthesized from a function-valued `__index`
/// metatable entry.
pub fn function_indexer_metatable(arena: &Arena, metatable: TypeId) -> Option<TableIndexer> {
    let TypeKind::Table(metatable) = arena.get(arena.follow(metatable)) else {
        return None;
    };
    let index = metatable.properties.get("__index")?.ty;
    let TypeKind::Function(FunctionType {
        arguments, returns, ..
    }) = arena.get(arena.follow(index))
    else {
        return None;
    };
    let arguments = arena.normalize_pack(*arguments);
    let key = arguments.types.get(1).copied()?;
    let value = arena.first_in_pack(*returns)?;
    Some(TableIndexer {
        key,
        value,
        read_only: true,
    })
}

/// Returns true when a read-only optional property may be omitted and still read
/// as `nil`.
pub fn missing_read_can_be_nil(arena: &Arena, property: &TableProperty) -> bool {
    property.read_only && !property.write_only && type_accepts_nil(arena, property.ty)
}

/// Nil acceptance used by relation solvers when judging optional properties.
pub fn type_accepts_nil(arena: &Arena, ty: TypeId) -> bool {
    let ty = arena.follow(ty);
    match arena.get(ty) {
        TypeKind::Primitive(PrimitiveType::Nil) => true,
        TypeKind::Any | TypeKind::Unknown | TypeKind::Error => true,
        TypeKind::Union(options) => options
            .iter()
            .any(|option| type_accepts_nil(arena, *option)),
        _ => false,
    }
}

/// Nil acceptance used by post-solve call/overload arity checks.
///
/// This deliberately treats open solver placeholders as nil-accepting and
/// handles intersections and negated nil, matching Luau's optional-argument
/// prefix rules. It is wider than generation-state expression nil checks,
/// which run before all constraints have been solved and must not infer nil
/// from free or blocked placeholders.
pub fn type_accepts_nil_for_arity(arena: &Arena, ty: TypeId) -> bool {
    type_accepts_nil_for_arity_seen(arena, ty, &mut BTreeSet::new())
}

fn type_accepts_nil_for_arity_seen(arena: &Arena, ty: TypeId, seen: &mut BTreeSet<TypeId>) -> bool {
    let ty = arena.follow(ty);
    if !seen.insert(ty) {
        return false;
    }
    match arena.get(ty) {
        TypeKind::Primitive(PrimitiveType::Nil)
        | TypeKind::Any
        | TypeKind::Unknown
        | TypeKind::Error
        | TypeKind::Blocked(_)
        | TypeKind::Free(_) => true,
        TypeKind::Union(options) => options
            .iter()
            .any(|option| type_accepts_nil_for_arity_seen(arena, *option, seen)),
        TypeKind::Intersection(options) => options
            .iter()
            .all(|option| type_accepts_nil_for_arity_seen(arena, *option, seen)),
        TypeKind::Negation(inner) => !matches!(
            arena.get(arena.follow(*inner)),
            TypeKind::Primitive(PrimitiveType::Nil)
        ),
        TypeKind::Bound(_) => unreachable!("follow removes bound types"),
        TypeKind::Primitive(_)
        | TypeKind::Singleton(_)
        | TypeKind::Function(_)
        | TypeKind::Table(_)
        | TypeKind::Metatable { .. }
        | TypeKind::Extern { .. }
        | TypeKind::TypeFunctionInstance { .. }
        | TypeKind::Generic(_)
        | TypeKind::Never => false,
    }
}

/// Returns true for dynamic types that can stand in for member access.
pub fn type_is_dynamic(arena: &Arena, ty: TypeId) -> bool {
    matches!(
        arena.get(arena.follow(ty)),
        TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_)
    )
}

/// Returns false for unresolved property modifiers whose variance cannot yet be
/// decided concretely.
pub fn property_modifier_is_concrete(arena: &Arena, ty: TypeId) -> bool {
    !matches!(
        arena.get(arena.follow(ty)),
        TypeKind::Free(_) | TypeKind::Generic(_) | TypeKind::Blocked(_)
    )
}

/// Returns true for the historical method-probe shape used to relax receiver
/// covariance.
pub fn method_probe_function_shape_matches(arena: &Arena, sub: TypeId, sup: TypeId) -> bool {
    let (TypeKind::Function(sub), TypeKind::Function(sup)) =
        (arena.get(arena.follow(sub)), arena.get(arena.follow(sup)))
    else {
        return false;
    };
    let sub_arguments = arena.normalize_pack(sub.arguments);
    let sup_arguments = arena.normalize_pack(sup.arguments);
    let sub_returns = arena.normalize_pack(sub.returns);
    let sup_returns = arena.normalize_pack(sup.returns);
    sub_arguments.types.len() == 1
        && sub_arguments.tail.is_none()
        && sup_arguments.types.len() == 1
        && sup_arguments.tail.is_none()
        && sub_returns.types.is_empty()
        && sub_returns.tail.is_none()
        && sup_returns.types.is_empty()
        && sup_returns.tail.is_none()
}

/// Returns a primitive-library property type for primitive member reads.
pub fn primitive_property_type(
    arena: &mut Arena,
    primitive: PrimitiveType,
    name: &str,
) -> Option<TypeId> {
    match primitive {
        PrimitiveType::String => string_primitive_property_type(arena, name),
        PrimitiveType::Vector => vector_primitive_property_type(arena, name),
        _ => None,
    }
}

#[cfg(any())]
mod tests;
