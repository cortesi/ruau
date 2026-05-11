//! Errors produced while traversing value boundaries.

use std::result::Result as StdResult;

use super::ValuePath;
use crate::Error;

/// Result type returned by value-boundary visitors.
pub type ValueVisitResult<T> = StdResult<T, ValueVisitError>;

/// A typed failure encountered while traversing a value boundary.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ValueVisitError {
    /// Traversal exceeded the maximum supported nesting depth.
    #[error("value nesting exceeds maximum depth of {max_depth} at {path}")]
    DepthLimit {
        /// Path to the value that exceeded the depth budget.
        path: ValuePath,
        /// Configured maximum depth.
        max_depth: usize,
    },
    /// A table cycle was found after host hooks declined to handle the table.
    #[error("cycle detected at {path}")]
    Cycle {
        /// Path to the repeated table.
        path: ValuePath,
    },
    /// An array table has a missing positive integer index.
    #[error("sparse array at {path}: missing index {index}")]
    SparseArray {
        /// Path to the array table.
        path: ValuePath,
        /// The missing 1-based array index.
        index: usize,
    },
    /// A table contains both positive integer keys and string keys.
    #[error(
        "mixed array/map table at {path}: found {found_key_type} key after {existing_key_type} keys"
    )]
    MixedTableKeys {
        /// Path to the table.
        path: ValuePath,
        /// The key family already seen in the table.
        existing_key_type: &'static str,
        /// The key family that conflicts with the existing table shape.
        found_key_type: &'static str,
    },
    /// A table key is not supported by the boundary rules.
    #[error("unsupported table key at {path}: {key_type}")]
    UnsupportedTableKey {
        /// Path to the table containing the key.
        path: ValuePath,
        /// Type name of the unsupported key.
        key_type: &'static str,
    },
    /// A string table key is not valid UTF-8.
    #[error("non-UTF-8 table key at {path}")]
    NonUtf8TableKey {
        /// Path to the table containing the key.
        path: ValuePath,
    },
    /// A value shape is not supported by the boundary rules.
    #[error("unsupported {type_name} at {path}")]
    UnsupportedValue {
        /// Path to the unsupported value.
        path: ValuePath,
        /// Type name of the unsupported value.
        type_name: &'static str,
    },
    /// Luau failed while constructing or reading a value.
    #[error("Luau error at {path}: {source}")]
    Luau {
        /// Path to the value being processed.
        path: ValuePath,
        /// Underlying Luau error.
        source: Error,
    },
    /// A host visitor rejected a value.
    #[error("{message} at {path}")]
    Custom {
        /// Path to the rejected value.
        path: ValuePath,
        /// Human-readable error message.
        message: String,
    },
}

impl ValueVisitError {
    /// Returns the path associated with this error.
    #[must_use]
    pub const fn path(&self) -> &ValuePath {
        match self {
            Self::DepthLimit { path, .. }
            | Self::Cycle { path }
            | Self::SparseArray { path, .. }
            | Self::MixedTableKeys { path, .. }
            | Self::UnsupportedTableKey { path, .. }
            | Self::NonUtf8TableKey { path }
            | Self::UnsupportedValue { path, .. }
            | Self::Luau { path, .. }
            | Self::Custom { path, .. } => path,
        }
    }

    /// Creates a path-attributed host error.
    #[must_use]
    pub fn custom(path: &ValuePath, message: impl Into<String>) -> Self {
        Self::Custom {
            path: path.clone(),
            message: message.into(),
        }
    }

    /// Wraps a Luau error at a traversal path.
    pub(super) fn luau(path: &ValuePath, source: Error) -> Self {
        Self::Luau {
            path: path.clone(),
            source,
        }
    }
}
