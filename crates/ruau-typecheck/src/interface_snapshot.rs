//! Frozen module-interface snapshot for cross-module reuse.
//!
//! Upstream's checker keeps two arenas per module — `internalTypes`
//! holds in-flight solver state, and `interfaceTypes` holds the
//! post-generalization public surface that other modules consume.
//! `clonePublicInterface` walks the elaborated module and copies the
//! exported value/type/return surface into the interface arena, so
//! later edits to `internalTypes` cannot mutate other modules'
//! exported types.
//!
//! `InterfaceSnapshot` is the matching Ruau-side skeleton. Today it
//! stores raw `TypeId`s from the session arena alongside their
//! rendered single-line summaries (so cross-module consumers can
//! diff exported surfaces without having to share a `Arena`).
//! A future dual-arena revision will own a frozen `Arena` and
//! return its `TypeId`s — at that point callers must translate
//! through the snapshot's own arena.

use std::collections::BTreeMap;

use crate::{
    checker::CheckedModule,
    types::{Arena, TypeId},
};

/// One frozen export entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenExport {
    /// Source-visible export name.
    pub name: String,
    /// Source arena handle for the exported type, when one was
    /// elaborated. Callers should treat this handle as opaque — its
    /// lifetime is the source `Checker` session.
    pub source_handle: Option<TypeId>,
    /// Deterministic single-line summary of the exported type at the
    /// time of the snapshot. Stable across solver edits because it
    /// captures the post-generalization rendering directly.
    pub summary: Option<String>,
}

/// Frozen module-return entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenReturn {
    /// Source arena handle for this return value.
    pub source_handle: TypeId,
    /// Deterministic single-line summary of the return type.
    pub summary: String,
}

/// Frozen snapshot of one checked module's public interface.
///
/// Take a snapshot once the module is fully generalized — re-running
/// the solver against later edits cannot change the snapshot's
/// rendered exports. Snapshots are cheap to clone and compare for
/// equality, which is the primary cache-invariant probe.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InterfaceSnapshot {
    /// Exported types, keyed by export name. The map preserves
    /// upstream's "exports are a name-indexed table" view.
    pub exported_types: BTreeMap<String, FrozenExport>,
    /// Module return surface, in declaration order.
    pub return_types: Vec<FrozenReturn>,
}

impl InterfaceSnapshot {
    /// Returns an empty snapshot. Useful for cases where a module
    /// has no exports yet (parse errors, `nocheck` mode, etc.).
    #[must_use]
    #[cfg(any())]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds a snapshot from a checked module plus the session arena
    /// the module's `TypeId`s belong to.
    ///
    /// The snapshot records both the source-arena handle and a
    /// rendered summary; consumers that care only about the rendered
    /// surface (cache-invariance probes, fixture comparisons) can
    /// ignore the handle. Consumers that need to resolve back into a
    /// type must hold the same `Checker` session that produced the
    /// snapshot.
    #[must_use]
    pub fn from_module(arena: &Arena, module: &CheckedModule) -> Self {
        let exported_types = module
            .exports()
            .types()
            .iter()
            .map(|(name, export)| {
                let frozen = FrozenExport {
                    name: name.clone(),
                    source_handle: export.ty,
                    summary: export.ty.map(|id| arena.summary(id)),
                };
                (name.clone(), frozen)
            })
            .collect();
        let return_types = module
            .return_types()
            .iter()
            .map(|id| FrozenReturn {
                source_handle: *id,
                summary: arena.summary(*id),
            })
            .collect();
        Self {
            exported_types,
            return_types,
        }
    }

    /// Returns true when the snapshot has no exports or returns.
    #[must_use]
    #[cfg(any())]
    pub fn is_empty(&self) -> bool {
        self.exported_types.is_empty() && self.return_types.is_empty()
    }
}

#[cfg(any())]
mod tests {
    use super::*;
    use crate::checker::Checker;

    #[test]
    fn snapshot_records_exports_and_returns() {
        let mut checker = Checker::new();
        let module = checker
            .check_source("export type Pair = { first: number, second: string }\nreturn { first = 1, second = \"x\" }");

        let snapshot = InterfaceSnapshot::from_module(checker.arena(), &module);

        assert!(snapshot.exported_types.contains_key("Pair"));
        let pair = &snapshot.exported_types["Pair"];
        assert_eq!(pair.name, "Pair");
        assert!(pair.summary.is_some());

        assert_eq!(snapshot.return_types.len(), 1);
        assert!(!snapshot.return_types[0].summary.is_empty());
    }

    #[test]
    fn empty_snapshot_is_empty() {
        let snapshot = InterfaceSnapshot::empty();
        assert!(snapshot.is_empty());
        assert!(snapshot.exported_types.is_empty());
        assert!(snapshot.return_types.is_empty());
    }
}
