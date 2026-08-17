//! Error types for declaration construction.

use std::{error::Error as StdError, fmt};

/// All validation errors reported by [`Builder::build`](crate::Builder::build).
///
/// Always holds at least one error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildError {
    errors: Vec<BuildErrorEntry>,
}

impl BuildError {
    /// Bundles validation errors.
    ///
    /// # Panics
    /// Panics when `errors` is empty: an error bundle must carry at least one
    /// error.
    pub(crate) fn new(errors: Vec<BuildErrorEntry>) -> Self {
        assert!(
            !errors.is_empty(),
            "a BuildError bundle must hold at least one error"
        );
        Self { errors }
    }

    /// Returns the collected errors in validation order. Never empty.
    #[must_use]
    pub fn errors(&self) -> &[BuildErrorEntry] {
        &self.errors
    }

    /// Consumes the error bundle and returns the collected errors. Never
    /// empty.
    #[must_use]
    pub fn into_errors(self) -> Vec<BuildErrorEntry> {
        self.errors
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.errors.as_slice() {
            [error] => write!(formatter, "{error}"),
            errors => {
                writeln!(
                    formatter,
                    "declaration validation failed with {} errors:",
                    errors.len()
                )?;
                for error in errors {
                    writeln!(formatter, "- {error}")?;
                }
                Ok(())
            }
        }
    }
}

impl StdError for BuildError {}

/// One declaration validation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildErrorEntry {
    /// A declaration, parameter, class field, or method name is not a Luau identifier.
    InvalidIdentifier {
        /// The declaration location that supplied the name.
        location: String,
        /// The invalid name.
        name: String,
    },
    /// Two items declared the same name with different shapes.
    ConflictingItem {
        /// The conflicting item name.
        name: String,
        /// The first rendered declaration body.
        first: String,
        /// The conflicting rendered declaration body.
        second: String,
    },
    /// A [`Type::Named`](crate::Type::Named) reference could not be resolved.
    UnknownType {
        /// The declaration location that referenced the type.
        location: String,
        /// The unresolved type name.
        name: String,
    },
}

impl fmt::Display for BuildErrorEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier { location, name } => {
                write!(formatter, "{location} has invalid Luau identifier `{name}`")
            }
            Self::ConflictingItem {
                name,
                first,
                second,
            } => {
                write!(
                    formatter,
                    "`{name}` is declared more than once with different bodies: \
                     `{first}` vs `{second}`"
                )
            }
            Self::UnknownType { location, name } => {
                write!(formatter, "{location} references unknown type `{name}`")
            }
        }
    }
}

impl StdError for BuildErrorEntry {}
