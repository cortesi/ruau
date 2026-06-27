//! Error types for declaration construction.

use std::{error::Error as StdError, fmt};

/// All validation errors reported by [`Builder::finish`](crate::Builder::finish).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Errors {
    errors: Vec<Error>,
}

impl Errors {
    pub(crate) fn new(errors: Vec<Error>) -> Self {
        Self { errors }
    }

    /// Returns the collected errors in validation order.
    #[must_use]
    pub fn errors(&self) -> &[Error] {
        &self.errors
    }

    /// Consumes the error bundle and returns the collected errors.
    #[must_use]
    pub fn into_errors(self) -> Vec<Error> {
        self.errors
    }

    /// Returns true when no errors are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }
}

impl fmt::Display for Errors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.errors.as_slice() {
            [] => formatter.write_str("declaration validation failed"),
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

impl StdError for Errors {}

/// One declaration validation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
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
    /// A [`Ty::Named`](crate::Ty::Named) reference could not be resolved.
    UnknownType {
        /// The declaration location that referenced the type.
        location: String,
        /// The unresolved type name.
        name: String,
    },
}

impl fmt::Display for Error {
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
