use std::path::PathBuf;

use thiserror::Error;

/// Failure to validate a filesystem source root.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum DirectoryError {
    /// The requested root could not be canonicalized.
    #[error("cannot canonicalize filesystem root {}: {message}", root.display())]
    CanonicalizeRoot {
        /// Requested root.
        root: PathBuf,
        /// I/O detail.
        message: String,
    },
    /// The canonical root is not a directory.
    #[error("filesystem source root is not a directory: {}", root.display())]
    RootNotDirectory {
        /// Canonical root.
        root: PathBuf,
    },
    /// The canonical root could not be opened as a capability.
    #[error("cannot open filesystem root {}: {message}", root.display())]
    OpenRoot {
        /// Canonical root.
        root: PathBuf,
        /// I/O detail.
        message: String,
    },
}
