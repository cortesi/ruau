//! Hard module-graph traversal limits.

use std::{fmt, num::NonZeroUsize};

use ruau_source::ModuleId;

/// Finite limits enforced while a source graph is traversed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphLimits {
    max_modules: NonZeroUsize,
    max_require_depth: NonZeroUsize,
    max_source_bytes: NonZeroUsize,
}

impl GraphLimits {
    /// Creates an invariant-bearing finite graph limit set.
    #[must_use]
    pub const fn new(
        max_modules: NonZeroUsize,
        max_require_depth: NonZeroUsize,
        max_source_bytes: NonZeroUsize,
    ) -> Self {
        Self {
            max_modules,
            max_require_depth,
            max_source_bytes,
        }
    }

    /// Returns the root-inclusive module-count limit.
    #[must_use]
    pub const fn max_modules(self) -> NonZeroUsize {
        self.max_modules
    }

    /// Returns the maximum number of require edges from the root.
    #[must_use]
    pub const fn max_require_depth(self) -> NonZeroUsize {
        self.max_require_depth
    }

    /// Returns the aggregate source-byte limit.
    #[must_use]
    pub const fn max_source_bytes(self) -> NonZeroUsize {
        self.max_source_bytes
    }
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self::new(
            NonZeroUsize::new(1_024).expect("constant is non-zero"),
            NonZeroUsize::new(64).expect("constant is non-zero"),
            NonZeroUsize::new(16 * 1024 * 1024).expect("constant is non-zero"),
        )
    }
}

/// Graph resource whose finite bound was exceeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphLimitKind {
    /// Number of canonical modules, including the root.
    Modules,
    /// Number of require edges from the root.
    RequireDepth,
    /// Aggregate bytes across canonical module sources.
    SourceBytes,
}

impl fmt::Display for GraphLimitKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Modules => formatter.write_str("modules"),
            Self::RequireDepth => formatter.write_str("require depth"),
            Self::SourceBytes => formatter.write_str("source bytes"),
        }
    }
}

/// Structured hard failure raised during bounded graph traversal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphLimitError {
    kind: GraphLimitKind,
    maximum: usize,
    observed: usize,
    module: ModuleId,
    requester: Option<ModuleId>,
}

impl GraphLimitError {
    pub(crate) fn new(
        kind: GraphLimitKind,
        maximum: usize,
        observed: usize,
        module: ModuleId,
        requester: Option<ModuleId>,
    ) -> Self {
        Self {
            kind,
            maximum,
            observed,
            module,
            requester,
        }
    }

    /// Returns the resource whose limit was exceeded.
    #[must_use]
    pub const fn kind(&self) -> GraphLimitKind {
        self.kind
    }

    /// Returns the configured maximum.
    #[must_use]
    pub const fn maximum(&self) -> usize {
        self.maximum
    }

    /// Returns the first rejected observation.
    #[must_use]
    pub const fn observed(&self) -> usize {
        self.observed
    }

    /// Returns the module responsible for the rejected observation.
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        &self.module
    }

    /// Returns the requester responsible for the rejected edge, when present.
    #[must_use]
    pub const fn requester(&self) -> Option<&ModuleId> {
        self.requester.as_ref()
    }
}

impl fmt::Display for GraphLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "graph {} limit exceeded at module '{}' (maximum {}, observed {})",
            self.kind, self.module, self.maximum, self.observed
        )?;
        if let Some(requester) = &self.requester {
            write!(formatter, " from requester '{requester}'")?;
        }
        Ok(())
    }
}

impl std::error::Error for GraphLimitError {}
