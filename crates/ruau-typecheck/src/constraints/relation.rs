use super::{ConstraintLocation, ConstraintSolveError, ConstraintSolver};
use crate::{
    subtype::{SubtypeTarget, Subtyper},
    type_function::{Reduction, TypeFunctionRuntime},
    types::{TypeId, TypeKind, TypePackId},
};

impl<'a> ConstraintSolver<'a> {
    pub(super) fn solve_unify(
        &mut self,
        left: TypeId,
        right: TypeId,
        location: ConstraintLocation,
    ) -> Result<(), ConstraintSolveError> {
        self.unifier()
            .unify(left, right)
            .map_err(ConstraintSolveError::Unify)
            .map_err(|error| location.apply(error))
    }

    pub(super) fn solve_subtype(
        &mut self,
        sub: TypeId,
        sup: TypeId,
        location: ConstraintLocation,
    ) -> Result<(), ConstraintSolveError> {
        self.merge_unsealed_table_assignment(sub, sup);
        self.require_subtype(sub, sup)
            .map_err(|error| location.apply(error))
    }

    pub(super) fn solve_pack_subtype(
        &mut self,
        sub: TypePackId,
        sup: TypePackId,
        location: ConstraintLocation,
    ) -> Result<(), ConstraintSolveError> {
        self.require_return_pack_subtype(sub, sup)
            .and_then(|()| {
                self.unifier()
                    .constrain_pack_subtype(sub, sup)
                    .map_err(ConstraintSolveError::Unify)
            })
            .map_err(|error| location.apply(error))
    }

    pub(super) fn require_subtype(
        &mut self,
        sub: TypeId,
        sup: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        let sub = self.reduce_type_function_instance(sub);
        let sup = self.reduce_type_function_instance(sup);
        match Subtyper::new(self.arena).is_subtype(sub, sup) {
            Ok(()) => Ok(()),
            Err(error) => {
                let suppression = Subtyper::new(self.arena).suppression(sub, sup);
                Err(ConstraintSolveError::SubtypeWithMetadata {
                    error: Box::new(error),
                    sub: SubtypeTarget::Type(sub),
                    sup: SubtypeTarget::Type(sup),
                    suppression,
                })
            }
        }
    }
    fn reduce_type_function_instance(&mut self, id: TypeId) -> TypeId {
        let id = self.arena.follow(id);
        let TypeKind::TypeFunctionInstance { name, arguments } = self.arena.get(id).clone() else {
            return id;
        };
        match TypeFunctionRuntime::new().reduce_allocating(self.arena, &name, &arguments) {
            Reduction::Reduced(reduced) if reduced != id => self.arena.follow(reduced),
            Reduction::Reduced(_) | Reduction::Pending => id,
        }
    }
    pub(super) fn require_pack_subtype(
        &self,
        sub: TypePackId,
        sup: TypePackId,
    ) -> Result<(), ConstraintSolveError> {
        match Subtyper::new(self.arena).is_subtype_pack(sub, sup) {
            Ok(()) => Ok(()),
            Err(error) => {
                let suppression = Subtyper::new(self.arena).pack_suppression(sub, sup);
                Err(ConstraintSolveError::SubtypeWithMetadata {
                    error: Box::new(error),
                    sub: SubtypeTarget::Pack(sub),
                    sup: SubtypeTarget::Pack(sup),
                    suppression,
                })
            }
        }
    }
    pub(super) fn require_return_pack_subtype(
        &self,
        sub: TypePackId,
        sup: TypePackId,
    ) -> Result<(), ConstraintSolveError> {
        match Subtyper::new(self.arena).is_subtype_return_pack(sub, sup) {
            Ok(()) => Ok(()),
            Err(error) => {
                let suppression = Subtyper::new(self.arena).pack_suppression(sub, sup);
                Err(ConstraintSolveError::SubtypeWithMetadata {
                    error: Box::new(error),
                    sub: SubtypeTarget::Pack(sub),
                    sup: SubtypeTarget::Pack(sup),
                    suppression,
                })
            }
        }
    }
    pub(super) fn merge_unsealed_table_assignment(&mut self, sub: TypeId, sup: TypeId) {
        let sub = self.arena.follow(sub);
        let sup = self.arena.follow(sup);
        let (TypeKind::Table(sub_table), TypeKind::Table(mut sup_table)) =
            (self.arena.get(sub).clone(), self.arena.get(sup).clone())
        else {
            return;
        };
        if !sup_table.is_unsealed() {
            return;
        }
        if sup_table.merge_unsealed_assignment(sub_table) {
            self.arena.replace(sup, TypeKind::Table(sup_table));
        }
    }
}
