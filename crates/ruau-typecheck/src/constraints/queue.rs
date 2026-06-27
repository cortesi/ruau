use super::{
    CallConstraintContext, Constraint, ConstraintKind, ConstraintSolveError,
    ConstraintSolveSummary, ConstraintSolver,
};
use crate::{
    type_function::{Reduction, TypeFunctionRuntime},
    types::{TypeId, TypeKind, TypePackId, TypePackKind},
};

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
        match &constraint.kind {
            ConstraintKind::Unify { left, right } => {
                self.type_is_blocked(*left)
                    || self.type_is_blocked(*right)
                    || self.type_function_waits_for_pending_operand(*left)
                    || self.type_function_waits_for_pending_operand(*right)
            }
            ConstraintKind::Subtype { sub, sup } => {
                self.type_is_blocked(*sub)
                    || self.type_is_blocked(*sup)
                    || self.type_function_waits_for_pending_operand(*sub)
                    || self.type_function_waits_for_pending_operand(*sup)
            }
            ConstraintKind::PackSubtype { sub, sup } => {
                self.pack_is_blocked(*sub) || self.pack_is_blocked(*sup)
            }
            ConstraintKind::Call {
                callee,
                arguments,
                context:
                    CallConstraintContext {
                        expected_returns, ..
                    },
            } => {
                self.type_is_blocked(*callee)
                    || self.pack_is_blocked(*arguments)
                    || expected_returns.is_some_and(|returns| self.pack_is_blocked(returns))
            }
            ConstraintKind::ReadIndexer {
                table, key, value, ..
            } => {
                self.type_is_blocked(*table)
                    || self.type_is_blocked(*key)
                    || self.type_is_blocked(*value)
            }
            ConstraintKind::ReadProperty { table, value, .. } => {
                self.type_is_blocked(*table) || self.type_is_blocked(*value)
            }
            ConstraintKind::WriteProperty { table, value, .. } => {
                self.type_is_blocked(*table) || self.type_is_blocked(*value)
            }
            ConstraintKind::WriteIndexer {
                table, key, value, ..
            } => {
                self.type_is_blocked(*table)
                    || self.type_is_blocked(*key)
                    || self.type_is_blocked(*value)
            }
        }
    }
    pub(super) fn type_is_blocked(&self, ty: TypeId) -> bool {
        self.type_is_blocked_with(ty, &mut Vec::new(), &mut Vec::new())
    }
    fn type_is_blocked_with(
        &self,
        mut ty: TypeId,
        seen_types: &mut Vec<TypeId>,
        seen_packs: &mut Vec<TypePackId>,
    ) -> bool {
        loop {
            if seen_types.contains(&ty) {
                return false;
            }
            seen_types.push(ty);
            match self.arena.get(ty) {
                TypeKind::Blocked(_) => return true,
                TypeKind::Bound(bound) => ty = *bound,
                TypeKind::Union(types) | TypeKind::Intersection(types) => {
                    return types
                        .iter()
                        .any(|ty| self.type_is_blocked_with(*ty, seen_types, seen_packs));
                }
                TypeKind::Negation(inner) => {
                    return self.type_is_blocked_with(*inner, seen_types, seen_packs);
                }
                TypeKind::Function(function) => {
                    return self.pack_is_blocked_with(function.arguments, seen_types, seen_packs)
                        || self.pack_is_blocked_with(function.returns, seen_types, seen_packs);
                }
                TypeKind::Table(table) => {
                    return table
                        .instantiated_type_params
                        .iter()
                        .chain(table.properties.values().map(|property| &property.ty))
                        .any(|ty| self.type_is_blocked_with(*ty, seen_types, seen_packs))
                        || table.indexer.iter().any(|indexer| {
                            self.type_is_blocked_with(indexer.key, seen_types, seen_packs)
                                || self.type_is_blocked_with(indexer.value, seen_types, seen_packs)
                        });
                }
                TypeKind::Metatable {
                    table, metatable, ..
                } => {
                    return self.type_is_blocked_with(*table, seen_types, seen_packs)
                        || self.type_is_blocked_with(*metatable, seen_types, seen_packs);
                }
                TypeKind::TypeFunctionInstance { arguments, .. } => {
                    return arguments
                        .iter()
                        .any(|ty| self.type_is_blocked_with(*ty, seen_types, seen_packs));
                }
                TypeKind::Primitive(_)
                | TypeKind::Singleton(_)
                | TypeKind::Extern { .. }
                | TypeKind::Free(_)
                | TypeKind::Generic(_)
                | TypeKind::Error
                | TypeKind::Unknown
                | TypeKind::Never
                | TypeKind::Any => return false,
            }
        }
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
                &mut Vec::new(),
                &mut Vec::new(),
            )
        })
    }
    fn type_contains_retryable_pending_operand(
        &self,
        ty: TypeId,
        seen_types: &mut Vec<TypeId>,
        seen_packs: &mut Vec<TypePackId>,
    ) -> bool {
        let ty = self.arena.follow(ty);
        if seen_types.contains(&ty) {
            return false;
        }
        seen_types.push(ty);
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
        seen_types: &mut Vec<TypeId>,
        seen_packs: &mut Vec<TypePackId>,
    ) -> bool {
        let pack = self.arena.follow_pack(pack);
        if seen_packs.contains(&pack) {
            return false;
        }
        seen_packs.push(pack);
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
    pub(super) fn pack_is_blocked(&self, pack: TypePackId) -> bool {
        self.pack_is_blocked_with(pack, &mut Vec::new(), &mut Vec::new())
    }
    fn pack_is_blocked_with(
        &self,
        mut pack: TypePackId,
        seen_types: &mut Vec<TypeId>,
        seen_packs: &mut Vec<TypePackId>,
    ) -> bool {
        loop {
            if seen_packs.contains(&pack) {
                return false;
            }
            seen_packs.push(pack);
            match self.arena.get_pack(pack) {
                TypePackKind::List { types, tail } => {
                    return types
                        .iter()
                        .any(|ty| self.type_is_blocked_with(*ty, seen_types, seen_packs))
                        || tail.is_some_and(|tail| {
                            self.pack_is_blocked_with(tail, seen_types, seen_packs)
                        });
                }
                TypePackKind::Variadic { ty } => {
                    return self.type_is_blocked_with(*ty, seen_types, seen_packs);
                }
                TypePackKind::Bound(bound) => pack = *bound,
                TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => {
                    return false;
                }
            }
        }
    }
}
