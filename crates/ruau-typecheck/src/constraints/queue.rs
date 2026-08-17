use std::collections::BTreeSet;

use super::{
    CallConstraintContext, Constraint, ConstraintKind, ConstraintSolveError,
    ConstraintSolveSummary, ConstraintSolver,
};
use crate::{
    type_function::{Reduction, TypeFunctionRuntime},
    types::{Arena, TypeId, TypeKind, TypePackId, TypePackKind},
};

const VISITING: u64 = 1;
const CLEAR: u64 = 2;
const BLOCKED: u64 = 3;
const STATE_BITS: u32 = 2;
const MAX_EPOCH: u64 = u64::MAX >> STATE_BITS;

#[derive(Default)]
pub(super) struct BlockedVisit {
    epoch: u64,
    types: Vec<u64>,
    packs: Vec<u64>,
}

impl BlockedVisit {
    fn start(&mut self) {
        if self.epoch == MAX_EPOCH {
            self.types.fill(0);
            self.packs.fill(0);
            self.epoch = 1;
        } else {
            self.epoch += 1;
        }
    }

    fn type_state(&self, ty: TypeId) -> u64 {
        self.state(&self.types, ty.index())
    }

    fn set_type_state(&mut self, ty: TypeId, state: u64) {
        Self::set_state(&mut self.types, ty.index(), self.epoch, state);
    }

    fn pack_state(&self, pack: TypePackId) -> u64 {
        self.state(&self.packs, pack.index())
    }

    fn set_pack_state(&mut self, pack: TypePackId, state: u64) {
        Self::set_state(&mut self.packs, pack.index(), self.epoch, state);
    }

    fn state(&self, entries: &[u64], index: usize) -> u64 {
        let entry = entries.get(index).copied().unwrap_or_default();
        if entry >> STATE_BITS == self.epoch {
            entry & ((1_u64 << STATE_BITS) - 1)
        } else {
            0
        }
    }

    fn set_state(entries: &mut Vec<u64>, index: usize, epoch: u64, state: u64) {
        if entries.len() <= index {
            entries.resize(index + 1, 0);
        }
        entries[index] = (epoch << STATE_BITS) | state;
    }
}

impl<'a> ConstraintSolver<'a> {
    /// Enqueues a constraint.
    pub fn push(&mut self, constraint: Constraint) {
        if let ConstraintKind::Subtype { sub, sup } = &constraint.kind {
            let sub_id = self.arena.follow(*sub);
            if matches!(self.arena.get(sub_id), TypeKind::Free(_))
                && matches!(
                    self.arena.get(self.arena.follow(*sup)),
                    TypeKind::Primitive(_) | TypeKind::Singleton(_)
                )
            {
                self.scalar_constrained_frees.insert(sub_id);
            }
        }
        self.pending.push_back(constraint);
    }
    /// Installs a cooperative cancellation flag polled by the solve loop.
    pub fn set_cancel_flag(&mut self, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.cancel = Some(cancel);
    }
    /// Whether the front-door request driving this solve has been abandoned.
    fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Relaxed))
    }
    /// Test helper: pending constraint count.
    #[cfg(any())]
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
    /// Test helper: requeue blocked constraints for another solve pass.
    #[cfg(any())]
    pub fn retry_blocked(&mut self) {
        self.pending.append(&mut self.blocked);
    }
    /// Test helper: solve with arena rollback on hard failure.
    #[cfg(any())]
    pub fn solve(&mut self) -> Result<ConstraintSolveSummary, ConstraintSolveError> {
        let checkpoint = self.arena.clone();
        let (summary, error) = self.solve_recovering();
        if let Some(error) = error {
            *self.arena = checkpoint;
            Err(error)
        } else {
            Ok(summary)
        }
    }
    /// Solves queued constraints while preserving successful mutations before
    /// the first hard failure.
    ///
    /// This is the checker recovery path: diagnostics should not erase useful
    /// inferred types that were established before a later constraint failed.
    #[cfg(any())]
    pub fn solve_recovering(&mut self) -> (ConstraintSolveSummary, Option<ConstraintSolveError>) {
        let (summary, errors) = self.solve_collecting();
        (
            summary,
            errors
                .into_iter()
                .find(|error| !error.is_fully_suppressing()),
        )
    }
    /// Drains the constraint queue, collecting *every* constraint
    /// failure rather than stopping at the first. Failed constraints
    /// do not interrupt subsequent ones — diagnostics are gathered
    /// across the whole queue. The iteration-limit failure is still
    /// returned as a single terminal error.
    pub fn solve_collecting(&mut self) -> (ConstraintSolveSummary, Vec<ConstraintSolveError>) {
        let mut solved = 0;
        let mut iterations = 0;
        let mut errors = Vec::new();
        let mut pass_made_progress = false;

        loop {
            let Some(constraint) = self.pending.pop_front() else {
                if pass_made_progress && !self.blocked.is_empty() {
                    self.pending.append(&mut self.blocked);
                    pass_made_progress = false;
                    continue;
                }
                break;
            };
            if iterations >= self.limits.max_iterations || self.is_cancelled() {
                errors.push(ConstraintSolveError::IterationLimit {
                    limit: self.limits.max_iterations,
                });
                return (
                    ConstraintSolveSummary {
                        solved,
                        blocked: self.blocked.len(),
                        iterations,
                    },
                    errors,
                );
            }
            iterations += 1;

            if self.is_blocked(&constraint) {
                self.blocked.push_back(constraint);
                continue;
            }

            match self.solve_one(constraint) {
                Ok(()) => {
                    solved += 1;
                    pass_made_progress = true;
                }
                Err(error) => error.append_flattened(&mut errors),
            }
        }

        (
            ConstraintSolveSummary {
                solved,
                blocked: self.blocked.len(),
                iterations,
            },
            errors,
        )
    }
    fn is_blocked(&mut self, constraint: &Constraint) -> bool {
        self.blocked_visit.start();
        match &constraint.kind {
            ConstraintKind::Unify { left, right } => {
                type_is_blocked_with(self.arena, *left, &mut self.blocked_visit)
                    || type_is_blocked_with(self.arena, *right, &mut self.blocked_visit)
                    || self.type_function_waits_for_pending_operand(*left)
                    || self.type_function_waits_for_pending_operand(*right)
            }
            ConstraintKind::Subtype { sub, sup } => {
                type_is_blocked_with(self.arena, *sub, &mut self.blocked_visit)
                    || type_is_blocked_with(self.arena, *sup, &mut self.blocked_visit)
                    || self.type_function_waits_for_pending_operand(*sub)
                    || self.type_function_waits_for_pending_operand(*sup)
            }
            ConstraintKind::PackSubtype { sub, sup } => {
                pack_is_blocked_with(self.arena, *sub, &mut self.blocked_visit)
                    || pack_is_blocked_with(self.arena, *sup, &mut self.blocked_visit)
            }
            ConstraintKind::Call {
                callee,
                arguments,
                context:
                    CallConstraintContext {
                        expected_returns, ..
                    },
            } => {
                type_is_blocked_with(self.arena, *callee, &mut self.blocked_visit)
                    || pack_is_blocked_with(self.arena, *arguments, &mut self.blocked_visit)
                    || expected_returns.is_some_and(|returns| {
                        pack_is_blocked_with(self.arena, returns, &mut self.blocked_visit)
                    })
            }
            ConstraintKind::ReadIndexer {
                table, key, value, ..
            } => {
                type_is_blocked_with(self.arena, *table, &mut self.blocked_visit)
                    || type_is_blocked_with(self.arena, *key, &mut self.blocked_visit)
                    || type_is_blocked_with(self.arena, *value, &mut self.blocked_visit)
            }
            ConstraintKind::ReadProperty { table, value, .. } => {
                type_is_blocked_with(self.arena, *table, &mut self.blocked_visit)
                    || type_is_blocked_with(self.arena, *value, &mut self.blocked_visit)
            }
            ConstraintKind::WriteProperty { table, value, .. } => {
                type_is_blocked_with(self.arena, *table, &mut self.blocked_visit)
                    || type_is_blocked_with(self.arena, *value, &mut self.blocked_visit)
            }
            ConstraintKind::WriteIndexer {
                table, key, value, ..
            } => {
                type_is_blocked_with(self.arena, *table, &mut self.blocked_visit)
                    || type_is_blocked_with(self.arena, *key, &mut self.blocked_visit)
                    || type_is_blocked_with(self.arena, *value, &mut self.blocked_visit)
            }
        }
    }
    #[cfg(any())]
    pub(super) fn type_is_blocked(&self, ty: TypeId) -> bool {
        let mut visit = BlockedVisit::default();
        visit.start();
        type_is_blocked_with(self.arena, ty, &mut visit)
    }
    fn type_function_waits_for_pending_operand(&mut self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        let TypeKind::TypeFunctionInstance { name, arguments } = self.arena.get(ty).clone() else {
            return false;
        };
        if !matches!(name.as_str(), "add" | "keyof" | "index") {
            return false;
        }

        let checkpoint = self.arena.checkpoint();
        let reduction = TypeFunctionRuntime::new().reduce_allocating(self.arena, &name, &arguments);
        self.arena.rollback_to(checkpoint);
        if reduction != Reduction::Pending {
            return false;
        }

        arguments.iter().any(|argument| {
            self.type_contains_retryable_pending_operand(
                *argument,
                &mut BTreeSet::new(),
                &mut BTreeSet::new(),
            )
        })
    }
    fn type_contains_retryable_pending_operand(
        &self,
        ty: TypeId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if !seen_types.insert(ty) {
            return false;
        }
        match self.arena.get(ty) {
            TypeKind::Free(_) | TypeKind::Blocked(_) => true,
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            TypeKind::Union(types)
            | TypeKind::Intersection(types)
            | TypeKind::TypeFunctionInstance {
                arguments: types, ..
            } => types.iter().any(|ty| {
                self.type_contains_retryable_pending_operand(*ty, seen_types, seen_packs)
            }),
            TypeKind::Negation(inner) => {
                self.type_contains_retryable_pending_operand(*inner, seen_types, seen_packs)
            }
            TypeKind::Function(function) => {
                self.pack_contains_retryable_pending_operand(
                    function.arguments,
                    seen_types,
                    seen_packs,
                ) || self.pack_contains_retryable_pending_operand(
                    function.returns,
                    seen_types,
                    seen_packs,
                )
            }
            TypeKind::Table(table) => {
                table
                    .instantiated_type_params
                    .iter()
                    .chain(table.properties.values().map(|property| &property.ty))
                    .any(|ty| {
                        self.type_contains_retryable_pending_operand(*ty, seen_types, seen_packs)
                    })
                    || table.indexer.iter().any(|indexer| {
                        self.type_contains_retryable_pending_operand(
                            indexer.key,
                            seen_types,
                            seen_packs,
                        ) || self.type_contains_retryable_pending_operand(
                            indexer.value,
                            seen_types,
                            seen_packs,
                        )
                    })
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_contains_retryable_pending_operand(*table, seen_types, seen_packs)
                    || self
                        .type_contains_retryable_pending_operand(*metatable, seen_types, seen_packs)
            }
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }
    fn pack_contains_retryable_pending_operand(
        &self,
        pack: TypePackId,
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
                    self.type_contains_retryable_pending_operand(*ty, seen_types, seen_packs)
                }) || tail.is_some_and(|tail| {
                    self.pack_contains_retryable_pending_operand(tail, seen_types, seen_packs)
                })
            }
            TypePackKind::Variadic { ty } => {
                self.type_contains_retryable_pending_operand(*ty, seen_types, seen_packs)
            }
            TypePackKind::Free { .. } => true,
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }
    #[cfg(any())]
    pub(super) fn pack_is_blocked(&self, pack: TypePackId) -> bool {
        let mut visit = BlockedVisit::default();
        visit.start();
        pack_is_blocked_with(self.arena, pack, &mut visit)
    }
}

fn type_is_blocked_with(arena: &Arena, ty: TypeId, visit: &mut BlockedVisit) -> bool {
    match visit.type_state(ty) {
        BLOCKED => return true,
        VISITING | CLEAR => return false,
        _ => {}
    }
    visit.set_type_state(ty, VISITING);
    let blocked = match arena.get(ty) {
        TypeKind::Blocked(_) => true,
        TypeKind::Bound(bound) => type_is_blocked_with(arena, *bound, visit),
        TypeKind::Union(types) | TypeKind::Intersection(types) => types
            .iter()
            .any(|ty| type_is_blocked_with(arena, *ty, visit)),
        TypeKind::Negation(inner) => type_is_blocked_with(arena, *inner, visit),
        TypeKind::Function(function) => {
            pack_is_blocked_with(arena, function.arguments, visit)
                || pack_is_blocked_with(arena, function.returns, visit)
        }
        TypeKind::Table(table) => {
            table
                .instantiated_type_params
                .iter()
                .chain(table.properties.values().flat_map(|property| {
                    std::iter::once(&property.ty).chain(property.write_ty.iter())
                }))
                .any(|ty| type_is_blocked_with(arena, *ty, visit))
                || table
                    .instantiated_type_pack_params
                    .iter()
                    .any(|pack| pack_is_blocked_with(arena, *pack, visit))
                || table.indexer.iter().any(|indexer| {
                    type_is_blocked_with(arena, indexer.key, visit)
                        || type_is_blocked_with(arena, indexer.value, visit)
                })
        }
        TypeKind::Metatable {
            table, metatable, ..
        } => {
            type_is_blocked_with(arena, *table, visit)
                || type_is_blocked_with(arena, *metatable, visit)
        }
        TypeKind::TypeFunctionInstance { arguments, .. } => arguments
            .iter()
            .any(|ty| type_is_blocked_with(arena, *ty, visit)),
        TypeKind::Extern {
            properties,
            indexer,
            ..
        } => {
            properties
                .values()
                .flat_map(|property| std::iter::once(&property.ty).chain(property.write_ty.iter()))
                .any(|ty| type_is_blocked_with(arena, *ty, visit))
                || indexer.iter().any(|indexer| {
                    type_is_blocked_with(arena, indexer.key, visit)
                        || type_is_blocked_with(arena, indexer.value, visit)
                })
        }
        TypeKind::Primitive(_)
        | TypeKind::Singleton(_)
        | TypeKind::Free(_)
        | TypeKind::Generic(_)
        | TypeKind::Error
        | TypeKind::Unknown
        | TypeKind::Never
        | TypeKind::Any => false,
    };
    visit.set_type_state(ty, if blocked { BLOCKED } else { CLEAR });
    blocked
}

fn pack_is_blocked_with(arena: &Arena, pack: TypePackId, visit: &mut BlockedVisit) -> bool {
    match visit.pack_state(pack) {
        BLOCKED => return true,
        VISITING | CLEAR => return false,
        _ => {}
    }
    visit.set_pack_state(pack, VISITING);
    let blocked = match arena.get_pack(pack) {
        TypePackKind::List { types, tail } => {
            types
                .iter()
                .any(|ty| type_is_blocked_with(arena, *ty, visit))
                || tail.is_some_and(|tail| pack_is_blocked_with(arena, tail, visit))
        }
        TypePackKind::Variadic { ty } => type_is_blocked_with(arena, *ty, visit),
        TypePackKind::Bound(bound) => pack_is_blocked_with(arena, *bound, visit),
        TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
    };
    visit.set_pack_state(pack, if blocked { BLOCKED } else { CLEAR });
    blocked
}

#[cfg(any())]
mod tests {
    use super::*;
    use crate::types::{BlockedType, TableProperty, TableState, TableType, TypeVariable};

    #[test]
    fn blocked_visit_epoch_invalidates_results_after_arena_mutation() {
        let mut arena = Arena::new();
        let free = arena.alloc(TypeKind::Free(TypeVariable {
            level: crate::types::TypeLevel(0),
            name: Some("T".to_owned()),
            lower_bound: None,
            upper_bound: None,
        }));
        let mut table = TableType::new(TableState::Sealed);
        table
            .properties
            .insert("value".to_owned(), TableProperty::new(free));
        let table = arena.alloc(TypeKind::Table(table));
        let mut visit = BlockedVisit::default();

        visit.start();
        assert!(!type_is_blocked_with(&arena, table, &mut visit));

        arena.replace(
            free,
            TypeKind::Blocked(BlockedType {
                reason: Some("waiting".to_owned()),
            }),
        );
        visit.start();
        assert!(type_is_blocked_with(&arena, table, &mut visit));
    }

    #[test]
    fn blocked_visit_includes_table_write_types_and_pack_parameters() {
        let mut arena = Arena::new();
        let blocked = arena.alloc(TypeKind::Blocked(BlockedType {
            reason: Some("waiting".to_owned()),
        }));
        let pack = arena.alloc_pack(TypePackKind::List {
            types: vec![blocked],
            tail: None,
        });
        let mut write_table = TableType::new(TableState::Sealed);
        let mut property = TableProperty::new(arena.primitives().string);
        property.write_ty = Some(blocked);
        write_table.properties.insert("value".to_owned(), property);
        let write_table = arena.alloc(TypeKind::Table(write_table));
        let mut pack_table = TableType::new(TableState::Sealed);
        pack_table.instantiated_type_pack_params.push(pack);
        let pack_table = arena.alloc(TypeKind::Table(pack_table));

        let mut visit = BlockedVisit::default();
        visit.start();
        assert!(type_is_blocked_with(&arena, write_table, &mut visit));
        visit.start();
        assert!(type_is_blocked_with(&arena, pack_table, &mut visit));
    }
}
