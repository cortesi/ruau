//! Type identity, type-pack, arena, and checked-module result scaffolding.
#![allow(clippy::multiple_inherent_impl)]

mod arena;
mod id;
mod kind;
mod path;
mod summary;
#[cfg(any())]
mod transaction;
#[cfg(any())]
mod traversal;

pub use arena::Arena;
pub(crate) use arena::FollowedTypeKind;
pub(crate) use id::TypeLevel;
#[cfg(any())]
pub(crate) use id::{ARENA_BOUNDARY, ArenaBoundary};
pub use id::{TableAliasIdentity, TypeId, TypePackId};
pub use kind::KindTag;
pub(crate) use kind::{
    BlockedType, FlattenedListPack, FunctionType, GenericType, GenericTypePack, NormalizedTypePack,
    PrimitiveType, PrimitiveTypes, SingletonType, TableIndexer, TableProperty, TableState,
    TableType, TypeKind, TypePackKind, TypePackTail, TypeVariable, alloc_top_function_type,
    extern_is_subtype, is_top_function_type, same_alias_identity_table_arity,
    same_alias_identity_table_instance, same_named_table_instance,
};
#[cfg(any())]
pub(crate) use path::TypePathBuilder;
pub(crate) use path::{
    PackField, PropertyAccess, TypeField, TypePath, TypePathComponent, TypePathRoot,
};
pub use summary::{FunctionSummaryOptions, SummaryOptions};
#[cfg(any())]
pub(crate) use transaction::TypeTransactionLog;
#[cfg(any())]
pub(crate) use traversal::TypeTraversalOptions;

#[cfg(any())]
mod tests;
