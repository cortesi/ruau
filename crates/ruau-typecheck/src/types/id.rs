//! Stable type and type-pack handles.

use serde::{Deserialize, Serialize};

/// Type-arena ownership boundary used by module checking.
#[cfg(any())]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArenaBoundary {
    /// One arena is owned by a checker session / checked frontend and shared by
    /// all modules checked in that session.
    CheckerSession,
}

#[cfg(any())]
impl ArenaBoundary {
    /// Human-readable rationale for the chosen boundary.
    #[must_use]
    pub const fn rationale(self) -> &'static str {
        match self {
            Self::CheckerSession => {
                "checked frontend sessions share one arena across modules; dirty invalidation clears or rebuilds session-owned types instead of translating handles between per-module arenas"
            }
        }
    }
}

/// Current arena-boundary decision for the type checker.
#[cfg(any())]
pub const ARENA_BOUNDARY: ArenaBoundary = ArenaBoundary::CheckerSession;

/// Stable handle for a type allocated in an arena.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TypeId(u32);

impl TypeId {
    /// Returns the zero-based arena index for this handle.
    #[must_use]
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }

    /// Creates a handle from a zero-based arena index.
    pub(crate) fn from_index(index: usize) -> Self {
        let index = u32::try_from(index).expect("type arena exceeded u32 handle space");
        Self(index)
    }
}

/// Stable handle for a type pack allocated in an arena.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TypePackId(u32);

impl TypePackId {
    /// Returns the zero-based arena index for this handle.
    #[must_use]
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }

    /// Creates a handle from a zero-based arena index.
    pub(super) fn from_index(index: usize) -> Self {
        let index = u32::try_from(index).expect("type-pack arena exceeded u32 handle space");
        Self(index)
    }
}

/// DCR level used for free and generic variables.
///
/// Keeping the representation here avoids baking raw integers into public
/// type summaries.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TypeLevel(pub u32);

/// Definition identity for a source type alias whose lowered result is a named
/// table.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TableAliasIdentity {
    /// Module that owns the alias definition, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Lexical scope that owns the alias definition within the module.
    pub scope: u32,
    /// Source-visible alias name at the definition site.
    pub name: String,
}
