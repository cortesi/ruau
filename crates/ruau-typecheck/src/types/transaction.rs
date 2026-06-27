//! Transaction log for staged type replacements.

use std::collections::BTreeMap;

#[cfg(any())]
use super::TypeLevel;
use super::{Arena, TypeId, TypeKind};

/// Pending type replacement recorded by [`TypeTransactionLog`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingTypeChange {
    /// Replacement type node.
    pub replacement: TypeKind,
    /// Whether this pending replacement was invalidated by a merge.
    pub dead: bool,
}

/// Transaction log for staging type-node replacements before commit.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TypeTransactionLog {
    /// Pending replacements keyed by destination type id.
    pending: BTreeMap<TypeId, PendingTypeChange>,
    /// Whether the log replaced a persistent type node.
    pub radioactive: bool,
}

impl TypeTransactionLog {
    /// Records a replacement.
    pub fn replace(&mut self, id: TypeId, replacement: TypeKind) {
        self.pending.insert(
            id,
            PendingTypeChange {
                replacement,
                dead: false,
            },
        );
    }

    /// Records a replacement of a persistent type node.
    #[cfg(any())]
    pub fn replace_persistent(&mut self, id: TypeId, replacement: TypeKind) {
        self.radioactive = true;
        self.replace(id, replacement);
    }

    /// Returns the pending replacement for `id`.
    #[must_use]
    pub fn pending(&self, id: TypeId) -> Option<&PendingTypeChange> {
        self.pending.get(&id)
    }

    /// Merges another transaction as a union alternative.
    ///
    /// Coincident replacements collapse to one pending change. When two bound
    /// replacements point at each other, the replacement rooted in the narrower
    /// scope wins, matching upstream's DCR collision rule.
    #[cfg(any())]
    pub fn concat_as_union(&mut self, other: Self, arena: &Arena) {
        for (incoming_id, incoming) in other.pending {
            if self.pending.get(&incoming_id) == Some(&incoming) {
                continue;
            }

            if let Some((existing_id, existing)) =
                self.find_reverse_bound_collision(incoming_id, &incoming)
            {
                if type_level(arena, incoming_id) > type_level(arena, existing_id) {
                    self.pending.remove(&existing_id);
                    self.pending.insert(incoming_id, incoming);
                } else {
                    let _ = existing;
                }
                continue;
            }

            self.pending.insert(incoming_id, incoming);
        }
    }

    /// Applies all pending replacements to the arena.
    pub fn commit(self, arena: &mut Arena) {
        for (id, change) in self.pending {
            if !change.dead {
                arena.replace(id, change.replacement);
            }
        }
    }

    /// Finds a pending `a -> b` replacement that collides with incoming
    /// `b -> a`.
    #[cfg(any())]
    fn find_reverse_bound_collision(
        &self,
        incoming_id: TypeId,
        incoming: &PendingTypeChange,
    ) -> Option<(TypeId, &PendingTypeChange)> {
        let TypeKind::Bound(incoming_target) = incoming.replacement else {
            return None;
        };

        self.pending.iter().find_map(|(existing_id, existing)| {
            if existing.replacement == TypeKind::Bound(incoming_id)
                && *existing_id == incoming_target
            {
                Some((*existing_id, existing))
            } else {
                None
            }
        })
    }
}

/// Returns a type variable level for transaction-log collision ordering.
#[cfg(any())]
fn type_level(arena: &Arena, id: TypeId) -> TypeLevel {
    match arena.get(id) {
        TypeKind::Free(variable) => variable.level,
        TypeKind::Generic(generic) => generic.level,
        _ => TypeLevel(0),
    }
}
