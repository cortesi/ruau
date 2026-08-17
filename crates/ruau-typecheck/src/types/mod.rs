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
    compatible_table_state, extern_is_subtype, is_top_function_type,
    negated_disjoint_primitives_cover_unknown, same_alias_identity_table_arity,
    same_alias_identity_table_instance, same_named_table_instance,
};
#[cfg(any())]
pub(crate) use path::TypePathBuilder;
pub(crate) use path::{PackField, TypeField, TypePath, TypePathComponent, TypePathRoot};
#[cfg(any())]
pub(crate) use summary::FunctionSummaryOptions;
pub use summary::SummaryOptions;
#[cfg(any())]
pub(crate) use transaction::TypeTransactionLog;
#[cfg(any())]
pub(crate) use traversal::TypeTraversalOptions;

pub(crate) use crate::diagnostics::PropertyAccess;

#[cfg(any())]
mod tests;
