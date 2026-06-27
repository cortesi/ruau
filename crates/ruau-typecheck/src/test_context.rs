//! Shared test harness for `ruau-typecheck`.
//!
//! `TestContext` owns a `Checker` (and therefore a `Arena` plus standard
//! builtins) and exposes the algorithm facades — subtype, unify, normalize,
//! simplify-pack — as `&mut self` methods together with a small kit of type
//! builders so unit tests don't have to repeat the same arena dance in every
//! file.
//!
//! The harness is `#[cfg(any())]` only and pub(crate).

#![cfg(test)]

use std::collections::BTreeMap;

use crate::{
    checker::{CheckedModule, Checker},
    normalize::Normalizer,
    subtype::{SubtypeError, Subtyper},
    types::{
        Arena, BlockedType, FunctionType, GenericType, SingletonType, TableProperty, TableState,
        TableType, TypeId, TypeKind, TypeLevel, TypePackId, TypePackKind, TypeVariable,
        alloc_top_function_type,
    },
    unify::{Unifier, UnifyError},
};

/// Convenience harness wrapping a `Checker` for unit tests.
///
/// The harness deliberately exposes algorithm facades (`assert_subtype`,
/// `unify`, `simplify_type`, etc.) instead of free functions so that:
///
/// * Tests share one arena/builtin environment with the same shape the
///   production checker uses.
/// * New helpers can be added in one place rather than duplicated across
///   `subtype_tests.rs`, `unify_tests.rs`, `normalize_tests.rs`, and so on.
pub struct TestContext {
    checker: Checker,
}

impl TestContext {
    /// Returns a context backed by a fresh `Checker` with the standard
    /// builtin environment.
    pub(crate) fn new() -> Self {
        Self {
            checker: Checker::new(),
        }
    }

    /// Returns a shared reference to the underlying type arena.
    pub(crate) fn arena(&self) -> &Arena {
        self.checker.arena()
    }

    /// Returns a mutable reference to the underlying type arena.
    pub(crate) fn arena_mut(&mut self) -> &mut Arena {
        self.checker.arena_mut()
    }

    /// Rendered single-line summary for `id`.
    pub(crate) fn summary(&self, id: TypeId) -> String {
        self.arena().summary(id)
    }

    // ── algorithm facades ────────────────────────────────────────────────

    /// Asserts `sub <: sup`, returning the error chain for failed cases.
    pub(crate) fn try_subtype(&self, sub: TypeId, sup: TypeId) -> Result<(), SubtypeError> {
        Subtyper::new(self.arena()).is_subtype(sub, sup)
    }

    /// Panics unless `sub <: sup`.
    pub(crate) fn assert_subtype(&self, sub: TypeId, sup: TypeId) {
        if let Err(error) = self.try_subtype(sub, sup) {
            panic!(
                "expected {} <: {}, but got {:?}",
                self.summary(sub),
                self.summary(sup),
                error
            );
        }
    }

    /// Panics unless `sub` is NOT a subtype of `sup`.
    pub(crate) fn assert_not_subtype(&self, sub: TypeId, sup: TypeId) {
        if self.try_subtype(sub, sup).is_ok() {
            panic!(
                "expected {} </: {}, but the relation held",
                self.summary(sub),
                self.summary(sup)
            );
        }
    }

    /// Subtype relation on type packs.
    pub(crate) fn try_subtype_pack(
        &self,
        sub: TypePackId,
        sup: TypePackId,
    ) -> Result<(), SubtypeError> {
        Subtyper::new(self.arena()).is_subtype_pack(sub, sup)
    }

    /// Unify two type handles in place.
    pub(crate) fn unify(&mut self, left: TypeId, right: TypeId) -> Result<(), UnifyError> {
        Unifier::new(self.arena_mut()).unify(left, right)
    }

    /// Unify two type-pack handles in place.
    pub(crate) fn unify_pack(
        &mut self,
        left: TypePackId,
        right: TypePackId,
    ) -> Result<(), UnifyError> {
        Unifier::new(self.arena_mut()).unify_pack(left, right)
    }

    /// Simplify a type without extern-negation expansion.
    pub(crate) fn simplify_type(&mut self, id: TypeId) -> TypeId {
        Normalizer::new(self.arena_mut()).simplify_type(id)
    }

    /// Simplify a type pack.
    pub(crate) fn simplify_pack(&mut self, id: TypePackId) -> TypePackId {
        Normalizer::new(self.arena_mut()).simplify_pack(id)
    }

    /// Check a Luau snippet through the wrapped `Checker`. Returns the
    /// resulting `CheckedModule` so callers can inspect diagnostics, scopes,
    /// or DFG state directly.
    pub(crate) fn check_snippet(&mut self, source: &str) -> CheckedModule {
        self.checker.check_source(source)
    }

    // ── type builders ────────────────────────────────────────────────────

    /// Allocates a fresh free type variable at level 0.
    pub(crate) fn free(&mut self, name: &str) -> TypeId {
        self.free_at(name, 0)
    }

    /// Allocates a free type variable at the given level.
    pub(crate) fn free_at(&mut self, name: &str, level: u32) -> TypeId {
        self.arena_mut().alloc(TypeKind::Free(TypeVariable {
            level: TypeLevel(level),
            name: Some(name.to_owned()),
            lower_bound: None,
            upper_bound: None,
        }))
    }

    /// Allocates a generic type at level 0 with the given display name.
    pub(crate) fn generic(&mut self, name: &str) -> TypeId {
        self.generic_at(name, 0)
    }

    /// Allocates a generic type at the given level.
    pub(crate) fn generic_at(&mut self, name: &str, level: u32) -> TypeId {
        self.arena_mut().alloc(TypeKind::Generic(GenericType {
            name: name.to_owned(),
            level: TypeLevel(level),
        }))
    }

    /// Allocates a `Blocked` placeholder type with the given debug reason.
    pub(crate) fn blocked(&mut self, reason: &str) -> TypeId {
        self.arena_mut().alloc(TypeKind::Blocked(BlockedType {
            reason: Some(reason.to_owned()),
        }))
    }

    /// Builds a fixed-length type pack from element types.
    pub(crate) fn list(&mut self, types: Vec<TypeId>) -> TypePackId {
        self.arena_mut()
            .alloc_pack(TypePackKind::List { types, tail: None })
    }

    /// Builds a fixed-length type pack with an explicit pack tail.
    pub(crate) fn list_with_tail(&mut self, types: Vec<TypeId>, tail: TypePackId) -> TypePackId {
        self.arena_mut().alloc_pack(TypePackKind::List {
            types,
            tail: Some(tail),
        })
    }

    /// Builds a variadic pack.
    pub(crate) fn variadic(&mut self, ty: TypeId) -> TypePackId {
        self.arena_mut().alloc_pack(TypePackKind::Variadic { ty })
    }

    /// Allocates a metatable type with explicit `table`/`metatable` components.
    pub(crate) fn metatable(&mut self, table: TypeId, metatable: TypeId) -> TypeId {
        self.arena_mut().alloc(TypeKind::Metatable {
            table,
            metatable,
            name: None,
        })
    }

    /// Allocates a metatable type with a display name.
    pub(crate) fn metatable_named(
        &mut self,
        name: impl Into<String>,
        table: TypeId,
        metatable: TypeId,
    ) -> TypeId {
        self.arena_mut().alloc(TypeKind::Metatable {
            table,
            metatable,
            name: Some(name.into()),
        })
    }

    /// Allocates a `TypeFunctionInstance { name, arguments }` placeholder.
    pub(crate) fn type_function_instance(
        &mut self,
        name: impl Into<String>,
        arguments: Vec<TypeId>,
    ) -> TypeId {
        self.arena_mut().alloc(TypeKind::TypeFunctionInstance {
            name: name.into(),
            arguments,
        })
    }

    /// Allocates a free type-pack variable at level 0.
    pub(crate) fn free_pack(&mut self, name: &str) -> TypePackId {
        self.free_pack_at(name, 0)
    }

    /// Allocates a free type-pack variable at the given level.
    pub(crate) fn free_pack_at(&mut self, name: &str, level: u32) -> TypePackId {
        self.arena_mut().alloc_pack(TypePackKind::Free {
            level: TypeLevel(level),
            name: Some(name.to_owned()),
        })
    }

    /// Returns the lower bound of a free type. Panics if the type is not free.
    pub(crate) fn free_lower_bound(&self, id: TypeId) -> Option<TypeId> {
        let TypeKind::Free(variable) = self.arena().get(id) else {
            panic!("expected free type at {id:?}");
        };
        variable.lower_bound
    }

    /// Returns the upper bound of a free type. Panics if the type is not free.
    pub(crate) fn free_upper_bound(&self, id: TypeId) -> Option<TypeId> {
        let TypeKind::Free(variable) = self.arena().get(id) else {
            panic!("expected free type at {id:?}");
        };
        variable.upper_bound
    }

    /// Applies a `sub <: sup` subtype constraint using DCR `Unifier::constrain_subtype`.
    pub(crate) fn constrain_subtype(&mut self, sub: TypeId, sup: TypeId) -> Result<(), UnifyError> {
        Unifier::new(self.arena_mut()).constrain_subtype(sub, sup)
    }

    /// Applies a pack-level `sub <: sup` constraint.
    pub(crate) fn constrain_pack_subtype(
        &mut self,
        sub: TypePackId,
        sup: TypePackId,
    ) -> Result<(), UnifyError> {
        Unifier::new(self.arena_mut()).constrain_pack_subtype(sub, sup)
    }

    /// Builds a function type from positional argument and return packs.
    pub(crate) fn function(&mut self, args: Vec<TypeId>, returns: Vec<TypeId>) -> TypeId {
        let arguments = self.list(args);
        let returns = self.list(returns);
        self.arena_mut()
            .alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
    }

    /// Builds a function type from already-allocated packs.
    pub(crate) fn function_from_packs(
        &mut self,
        arguments: TypePackId,
        returns: TypePackId,
    ) -> TypeId {
        self.arena_mut()
            .alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
    }

    /// Builds the universal top-function type `(...any) -> ...any`.
    pub(crate) fn top_function(&mut self) -> TypeId {
        alloc_top_function_type(self.arena_mut())
    }

    /// Builds an extern type root with an optional list of parent names.
    pub(crate) fn extern_type(&mut self, name: &str, parents: &[&str]) -> TypeId {
        self.arena_mut().alloc(TypeKind::Extern {
            name: name.to_owned(),
            parents: parents.iter().map(|p| (*p).to_owned()).collect(),
            properties: BTreeMap::new(),
            indexer: None,
        })
    }

    /// Builds an extern type with structural read properties.
    pub(crate) fn extern_type_with_properties(
        &mut self,
        name: &str,
        parents: &[&str],
        properties: &[(&str, TableProperty)],
    ) -> TypeId {
        self.arena_mut().alloc(TypeKind::Extern {
            name: name.to_owned(),
            parents: parents.iter().map(|p| (*p).to_owned()).collect(),
            properties: properties
                .iter()
                .map(|(property_name, property)| ((*property_name).to_owned(), property.clone()))
                .collect(),
            indexer: None,
        })
    }

    /// Builds an empty sealed table.
    pub(crate) fn table_sealed(&mut self) -> TypeId {
        self.arena_mut()
            .alloc(TypeKind::Table(TableType::new(TableState::Sealed)))
    }

    /// Builds an empty unsealed table.
    pub(crate) fn table_unsealed(&mut self) -> TypeId {
        self.arena_mut()
            .alloc(TypeKind::Table(TableType::new(TableState::Unsealed)))
    }

    /// Builds a sealed table populated with `(name, ty)` properties.
    pub(crate) fn table_with(&mut self, properties: &[(&str, TypeId)]) -> TypeId {
        let mut table = TableType::new(TableState::Sealed);
        for (name, ty) in properties {
            table
                .properties
                .insert((*name).to_owned(), TableProperty::new(*ty));
        }
        self.arena_mut().alloc(TypeKind::Table(table))
    }

    /// Builds a sealed table with one read-only property.
    pub(crate) fn table_with_readonly(&mut self, name: &str, ty: TypeId) -> TypeId {
        let mut table = TableType::new(TableState::Sealed);
        table.properties.insert(
            name.to_owned(),
            TableProperty {
                location: None,
                ty,
                write_ty: None,
                documentation_symbol: None,
                read_only: true,
                write_only: false,
                deprecated: false,
            },
        );
        self.arena_mut().alloc(TypeKind::Table(table))
    }

    /// Builds a singleton string type.
    pub(crate) fn singleton_string(&mut self, value: &str) -> TypeId {
        self.arena_mut()
            .alloc(TypeKind::Singleton(SingletonType::String(value.to_owned())))
    }

    /// Builds a singleton boolean type.
    pub(crate) fn singleton_bool(&mut self, value: bool) -> TypeId {
        self.arena_mut()
            .alloc(TypeKind::Singleton(SingletonType::Boolean(value)))
    }

    /// Builds a union type.
    pub(crate) fn union(&mut self, options: Vec<TypeId>) -> TypeId {
        self.arena_mut().alloc(TypeKind::Union(options))
    }

    /// Builds an intersection type.
    pub(crate) fn intersection(&mut self, options: Vec<TypeId>) -> TypeId {
        self.arena_mut().alloc(TypeKind::Intersection(options))
    }

    /// Builds a negation type.
    pub(crate) fn negation(&mut self, ty: TypeId) -> TypeId {
        self.arena_mut().alloc(TypeKind::Negation(ty))
    }
}

impl Default for TestContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any())]
mod tests {
    use super::*;
    use crate::types::TypeKind;

    #[test]
    fn assert_subtype_passes_for_primitives() {
        let ctx = TestContext::new();
        let number = ctx.arena().primitives().number;
        let any = ctx.arena().primitives().any;
        ctx.assert_subtype(number, any);
    }

    #[test]
    fn assert_not_subtype_fires_for_disjoint_primitives() {
        let ctx = TestContext::new();
        let number = ctx.arena().primitives().number;
        let string = ctx.arena().primitives().string;
        ctx.assert_not_subtype(number, string);
    }

    #[test]
    fn unify_binds_free_variable() {
        let mut ctx = TestContext::new();
        let variable = ctx.free("T");
        let number = ctx.arena().primitives().number;
        ctx.unify(variable, number).expect("free variable unifies");
        assert_eq!(ctx.arena().get(variable), &TypeKind::Bound(number));
    }

    #[test]
    fn table_with_builds_sealed_table() {
        let mut ctx = TestContext::new();
        let number = ctx.arena().primitives().number;
        let table = ctx.table_with(&[("count", number)]);
        match ctx.arena().get(table) {
            TypeKind::Table(value) => {
                assert!(value.is_sealed());
                assert_eq!(value.properties.len(), 1);
                assert_eq!(
                    value.properties.get("count").expect("count present").ty,
                    number
                );
            }
            other => panic!("expected a Table, got {other:?}"),
        }
    }

    #[test]
    fn function_builder_produces_function_type() {
        let mut ctx = TestContext::new();
        let number = ctx.arena().primitives().number;
        let string = ctx.arena().primitives().string;
        let function = ctx.function(vec![number], vec![string]);
        assert!(matches!(ctx.arena().get(function), TypeKind::Function(_)));
    }

    #[test]
    fn check_snippet_round_trips_an_empty_module() {
        let mut ctx = TestContext::new();
        let module = ctx.check_snippet("local n = 1");
        assert!(module.diagnostics().is_empty(), "snippet has no errors");
    }
}
