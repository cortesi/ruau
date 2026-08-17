//! Validated multi-root filesystem module sources.

use std::{
    error::Error as StdError,
    fmt, fs, io,
    path::{Path, PathBuf},
    str,
    sync::Arc,
};

use ruau_source::{
    InstanceKey, ModuleId, Mounts, ReadContext, Source, SourceFuture, SourceMetadata,
    SourceProvider, SourceRead,
};

use crate::{
    DEFAULT_MAX_READ_BYTES, Directory, DirectoryError, OpenedFile, SourceEpoch,
    module_name_escapes_root,
};

/// Builder for a validated [`DirectoryMounts`] source.
#[derive(Clone, Debug)]
pub struct DirectoryMountsBuilder {
    mounts: Vec<MountSpec>,
    max_read_bytes: usize,
}

#[derive(Clone, Debug)]
struct MountSpec {
    prefix: ModuleId,
    root: PathBuf,
}

impl DirectoryMountsBuilder {
    /// Creates an empty mount builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mounts: Vec::new(),
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
        }
    }

    /// Adds a module-id prefix and filesystem root.
    #[must_use]
    pub fn mount(mut self, prefix: impl AsRef<str>, root: impl Into<PathBuf>) -> Self {
        self.mounts.push(MountSpec {
            prefix: ModuleId::new(prefix.as_ref().as_bytes().to_vec()),
            root: root.into(),
        });
        self
    }

    /// Sets the per-file read cap used by every mount.
    #[must_use]
    pub fn max_read_bytes(mut self, max_read_bytes: usize) -> Self {
        self.max_read_bytes = max_read_bytes;
        self
    }

    /// Validates and builds the mounted source.
    ///
    /// Roots are canonicalized and must already exist as directories. Prefixes
    /// are canonicalized as portable module ids and must be non-empty,
    /// non-overlapping, and free of root traversal.
    ///
    /// # Errors
    /// Returns [`DirectoryMountsError`] for invalid prefixes or roots.
    pub fn build(self) -> Result<DirectoryMounts, DirectoryMountsError> {
        if self.mounts.is_empty() {
            return Err(DirectoryMountsError::NoMounts);
        }

        let mut entries = Vec::<FilesystemMount>::with_capacity(self.mounts.len());
        for spec in self.mounts {
            let prefix = normalize_prefix(&spec.prefix)?;
            for entry in &entries {
                if entry.prefix == prefix {
                    return Err(DirectoryMountsError::DuplicatePrefix { prefix });
                }
                if prefixes_overlap(&entry.prefix, &prefix) {
                    return Err(DirectoryMountsError::OverlappingPrefixes {
                        first: entry.prefix.clone(),
                        second: prefix,
                    });
                }
            }

            let epoch = SourceEpoch::default();
            let directory = Directory::with_epoch_handle(&spec.root, epoch.clone())
                .map_err(mount_error_from_directory)?
                .with_max_read_bytes(self.max_read_bytes);
            let root = directory.root().to_path_buf();
            if let Some(existing) = entries.iter().find(|entry| entry.root == root) {
                return Err(DirectoryMountsError::DuplicateRoot {
                    first_prefix: existing.prefix.clone(),
                    second_prefix: prefix,
                    root,
                });
            }
            entries.push(FilesystemMount {
                prefix,
                root,
                epoch,
                directory,
            });
        }

        entries.sort_by(|left, right| left.prefix.cmp(&right.prefix));
        let mut source = Mounts::builder().build();
        for entry in &entries {
            let child: Arc<dyn SourceProvider> = Arc::new(entry.directory.clone());
            source.insert(entry.prefix.clone(), child, entry.epoch.clone());
        }
        Ok(DirectoryMounts {
            source,
            mounts: entries,
            max_read_bytes: self.max_read_bytes,
        })
    }
}

impl Default for DirectoryMountsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Validated multi-root filesystem [`SourceProvider`].
///
/// Prefix dispatch delegates to [`Mounts`]; each child remains an
/// independently usable [`Directory`]. Reverse lookup canonicalizes
/// paths and rejects symlink escapes or paths that lie under more than one
/// nested root.
#[derive(Clone, Debug)]
pub struct DirectoryMounts {
    source: Mounts,
    mounts: Vec<FilesystemMount>,
    max_read_bytes: usize,
}

#[derive(Clone, Debug)]
struct FilesystemMount {
    prefix: ModuleId,
    root: PathBuf,
    epoch: SourceEpoch,
    directory: Directory,
}

#[derive(Debug)]
struct MountedFile<'a> {
    mount: &'a FilesystemMount,
    file: OpenedFile,
}

impl MountedFile<'_> {
    fn module_id(&self) -> Result<ModuleId, DirectoryMountsError> {
        let extension = self
            .file
            .origin()
            .extension()
            .and_then(|value| value.to_str());
        if !matches!(extension, Some("luau" | "lua")) {
            return Err(DirectoryMountsError::UnsupportedExtension {
                path: self.file.origin().to_path_buf(),
            });
        }

        let mut relative = self
            .file
            .origin()
            .strip_prefix(&self.mount.root)
            .expect("opened file origin remains inside its selected mount")
            .to_path_buf();
        relative.set_extension("");
        if relative.file_name().and_then(|name| name.to_str()) == Some("init") {
            relative.pop();
        }
        let inner = path_to_module_name(&relative)?;
        Ok(prefix_module_id(&self.mount.prefix, &inner))
    }
}

impl DirectoryMounts {
    /// Starts a validated mount builder.
    #[must_use]
    pub fn builder() -> DirectoryMountsBuilder {
        DirectoryMountsBuilder::new()
    }

    /// Composite epoch covering every mount prefix, child source, and
    /// mount-local invalidation handle.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        SourceProvider::epoch(&self.source)
    }

    /// Advances one mount's epoch and returns the new composite epoch.
    ///
    /// # Errors
    /// Returns [`DirectoryMountsError::UnknownPrefix`] when no mount matches.
    pub fn invalidate(&self, prefix: impl AsRef<str>) -> Result<u64, DirectoryMountsError> {
        let prefix = normalize_prefix(&ModuleId::new(prefix.as_ref().as_bytes().to_vec()))?;
        let Some(mount) = self.mounts.iter().find(|mount| mount.prefix == prefix) else {
            return Err(DirectoryMountsError::UnknownPrefix { prefix });
        };
        mount.epoch.bump();
        Ok(self.epoch())
    }

    /// Advances every mount epoch and returns the new composite epoch.
    pub fn invalidate_all(&self) -> u64 {
        for mount in &self.mounts {
            mount.epoch.bump();
        }
        self.epoch()
    }

    /// Maps an existing `.luau` or `.lua` file back to its mounted module id.
    ///
    /// `init.luau` and `init.lua` map to their parent module because that is
    /// the identity used by [`Directory`] for directory modules.
    /// Symlinks are canonicalized; one that resolves outside every root is
    /// rejected, and a path under nested roots is ambiguous rather than chosen
    /// by insertion order.
    ///
    /// # Errors
    /// Returns [`DirectoryMountsError`] for nonexistent, unsupported, outside,
    /// or ambiguous paths.
    pub fn module_id_for_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<ModuleId, DirectoryMountsError> {
        self.open_path(path.as_ref())?.module_id()
    }

    /// Reads an existing filesystem file as one graph-root observation.
    ///
    /// The returned value couples mounted runtime identity and epoch with the
    /// opened bytes, diagnostic metadata, and lossless final origin.
    ///
    /// # Errors
    /// Returns [`DirectoryMountsError`] as for [`Self::module_id_for_path`],
    /// plus bounded-read and UTF-8 failures.
    pub fn source_for_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<SourceRead, DirectoryMountsError> {
        let epoch = self.epoch();
        let opened = self.open_path(path.as_ref())?;
        let id = opened.module_id()?;
        let origin = opened.file.origin().to_path_buf();
        let bytes = opened
            .file
            .read_bounded(self.max_read_bytes)
            .map_err(|error| DirectoryMountsError::Read {
                path: origin.clone(),
                message: error.to_string(),
            })?;
        str::from_utf8(&bytes).map_err(|error| DirectoryMountsError::Read {
            path: origin.clone(),
            message: format!("source is not UTF-8: {error}"),
        })?;
        let observed = self.epoch();
        if observed != epoch {
            return Err(DirectoryMountsError::SourceChanged {
                expected: epoch,
                observed,
            });
        }
        let source = Source::bytes(id.clone(), bytes)
            .with_metadata(SourceMetadata::new(origin.to_string_lossy().into_owned()));
        Ok(SourceRead::new(
            source,
            InstanceKey::shared(id),
            epoch,
            Some(origin),
        ))
    }

    fn open_path(&self, requested: &Path) -> Result<MountedFile<'_>, DirectoryMountsError> {
        let canonical = fs::canonicalize(requested).map_err(|error| {
            DirectoryMountsError::CanonicalizePath {
                path: requested.to_path_buf(),
                message: error.to_string(),
            }
        })?;
        let matches = self
            .mounts
            .iter()
            .filter_map(|mount| {
                canonical
                    .strip_prefix(&mount.root)
                    .ok()
                    .map(|path| (mount, path))
            })
            .collect::<Vec<_>>();
        let [(mount, relative)] = matches.as_slice() else {
            return Err(path_match_error(canonical.clone(), &matches));
        };
        let file = match mount.directory.open_path(relative) {
            Ok(Some(file)) => file,
            Ok(None) => return Err(DirectoryMountsError::PathNotFile { path: canonical }),
            Err(error) => {
                return Err(open_path_error(mount, canonical, &error));
            }
        };

        let final_matches = self
            .mounts
            .iter()
            .filter_map(|candidate| {
                file.origin()
                    .strip_prefix(&candidate.root)
                    .ok()
                    .map(|path| (candidate, path))
            })
            .collect::<Vec<_>>();
        let [(final_mount, _)] = final_matches.as_slice() else {
            return Err(path_match_error(
                file.origin().to_path_buf(),
                &final_matches,
            ));
        };
        if final_mount.prefix != mount.prefix {
            return Err(DirectoryMountsError::OutsideRoots {
                path: file.origin().to_path_buf(),
            });
        }
        Ok(MountedFile { mount, file })
    }
}

impl SourceProvider for DirectoryMounts {
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId> {
        self.source.resolve(requester, request)
    }

    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
        self.source.read(id)
    }

    fn read_request(&self, request: ReadContext<'_>) -> SourceFuture<Vec<u8>> {
        self.source.read_request(request)
    }

    fn read_observation(&self, request: ReadContext<'_>) -> SourceFuture<SourceRead> {
        self.source.read_observation(request)
    }

    fn instance_key(&self, request: ReadContext<'_>) -> InstanceKey {
        self.source.instance_key(request)
    }

    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        self.source.metadata(id)
    }

    fn epoch(&self) -> u64 {
        self.epoch()
    }
}

/// Validation, reverse-lookup, or root-read failure for [`DirectoryMounts`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DirectoryMountsError {
    /// No mounts were configured.
    NoMounts,
    /// A prefix normalized to an empty id.
    EmptyPrefix,
    /// A prefix was not UTF-8 or attempted root traversal.
    InvalidPrefix {
        /// Original prefix.
        prefix: ModuleId,
    },
    /// Two prefixes normalize to the same id.
    DuplicatePrefix {
        /// Duplicate normalized prefix.
        prefix: ModuleId,
    },
    /// One prefix is a component-prefix of another.
    OverlappingPrefixes {
        /// First prefix.
        first: ModuleId,
        /// Second prefix.
        second: ModuleId,
    },
    /// A root could not be canonicalized.
    CanonicalizeRoot {
        /// Requested root.
        root: PathBuf,
        /// I/O detail.
        message: String,
    },
    /// A canonical root was not a directory.
    RootNotDirectory {
        /// Canonical root.
        root: PathBuf,
    },
    /// Two prefixes point at the same canonical root.
    DuplicateRoot {
        /// First prefix.
        first_prefix: ModuleId,
        /// Second prefix.
        second_prefix: ModuleId,
        /// Duplicate canonical root.
        root: PathBuf,
    },
    /// No configured mount has this normalized prefix.
    UnknownPrefix {
        /// Requested prefix.
        prefix: ModuleId,
    },
    /// A reverse-lookup path could not be canonicalized.
    CanonicalizePath {
        /// Requested path.
        path: PathBuf,
        /// I/O detail.
        message: String,
    },
    /// A reverse-lookup path was not a file.
    PathNotFile {
        /// Canonical path.
        path: PathBuf,
    },
    /// A file did not use `.luau` or `.lua`.
    UnsupportedExtension {
        /// Canonical path.
        path: PathBuf,
    },
    /// A canonical path lies outside every configured root.
    OutsideRoots {
        /// Canonical path.
        path: PathBuf,
    },
    /// A canonical path lies below multiple nested roots.
    AmbiguousRoots {
        /// Canonical path.
        path: PathBuf,
        /// Matching prefixes.
        prefixes: Vec<ModuleId>,
    },
    /// A relative path contained non-UTF-8 components.
    NonUtf8Path {
        /// Relative path.
        path: PathBuf,
    },
    /// Reading a root source failed.
    Read {
        /// Canonical file path.
        path: PathBuf,
        /// Read or UTF-8 detail.
        message: String,
    },
    /// A mount epoch changed while a root source was opened and read.
    SourceChanged {
        /// Epoch observed before opening the source.
        expected: u64,
        /// Epoch observed after reading the source.
        observed: u64,
    },
}

impl fmt::Display for DirectoryMountsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMounts => formatter.write_str("at least one filesystem mount is required"),
            Self::EmptyPrefix => formatter.write_str("filesystem mount prefix cannot be empty"),
            Self::InvalidPrefix { prefix } => {
                write!(formatter, "invalid filesystem mount prefix '{prefix}'")
            }
            Self::DuplicatePrefix { prefix } => {
                write!(formatter, "duplicate filesystem mount prefix '{prefix}'")
            }
            Self::OverlappingPrefixes { first, second } => {
                write!(
                    formatter,
                    "overlapping filesystem mount prefixes '{first}' and '{second}'"
                )
            }
            Self::CanonicalizeRoot { root, message } => {
                write!(
                    formatter,
                    "cannot canonicalize root {}: {message}",
                    root.display()
                )
            }
            Self::RootNotDirectory { root } => {
                write!(
                    formatter,
                    "filesystem mount root is not a directory: {}",
                    root.display()
                )
            }
            Self::DuplicateRoot { root, .. } => {
                write!(
                    formatter,
                    "duplicate filesystem mount root: {}",
                    root.display()
                )
            }
            Self::UnknownPrefix { prefix } => {
                write!(formatter, "unknown filesystem mount prefix '{prefix}'")
            }
            Self::CanonicalizePath { path, message } => {
                write!(
                    formatter,
                    "cannot canonicalize path {}: {message}",
                    path.display()
                )
            }
            Self::PathNotFile { path } => {
                write!(
                    formatter,
                    "filesystem source path is not a file: {}",
                    path.display()
                )
            }
            Self::UnsupportedExtension { path } => {
                write!(
                    formatter,
                    "filesystem source must use .luau or .lua: {}",
                    path.display()
                )
            }
            Self::OutsideRoots { path } => {
                write!(
                    formatter,
                    "filesystem source is outside mounted roots: {}",
                    path.display()
                )
            }
            Self::AmbiguousRoots { path, .. } => {
                write!(
                    formatter,
                    "filesystem source matches multiple mounted roots: {}",
                    path.display()
                )
            }
            Self::NonUtf8Path { path } => {
                write!(
                    formatter,
                    "filesystem source path is not UTF-8: {}",
                    path.display()
                )
            }
            Self::Read { path, message } => {
                write!(
                    formatter,
                    "cannot read filesystem source {}: {message}",
                    path.display()
                )
            }
            Self::SourceChanged { expected, observed } => write!(
                formatter,
                "filesystem source changed while reading (expected epoch {expected}, observed {observed})"
            ),
        }
    }
}

impl StdError for DirectoryMountsError {}

fn path_match_error(path: PathBuf, matches: &[(&FilesystemMount, &Path)]) -> DirectoryMountsError {
    if matches.is_empty() {
        DirectoryMountsError::OutsideRoots { path }
    } else {
        DirectoryMountsError::AmbiguousRoots {
            path,
            prefixes: matches
                .iter()
                .map(|(mount, _)| mount.prefix.clone())
                .collect(),
        }
    }
}

fn open_path_error(
    mount: &FilesystemMount,
    path: PathBuf,
    error: &io::Error,
) -> DirectoryMountsError {
    if let Ok(origin) = fs::canonicalize(&path)
        && !origin.starts_with(&mount.root)
    {
        return DirectoryMountsError::OutsideRoots { path: origin };
    }
    DirectoryMountsError::CanonicalizePath {
        path,
        message: error.to_string(),
    }
}

fn mount_error_from_directory(error: DirectoryError) -> DirectoryMountsError {
    match error {
        DirectoryError::CanonicalizeRoot { root, message }
        | DirectoryError::OpenRoot { root, message } => {
            DirectoryMountsError::CanonicalizeRoot { root, message }
        }
        DirectoryError::RootNotDirectory { root } => {
            DirectoryMountsError::RootNotDirectory { root }
        }
    }
}

fn normalize_prefix(prefix: &ModuleId) -> Result<ModuleId, DirectoryMountsError> {
    let Some(text) = prefix.as_str() else {
        return Err(DirectoryMountsError::InvalidPrefix {
            prefix: prefix.clone(),
        });
    };
    let normalized = ModuleId::canonicalized(text);
    let Some(normalized_text) = normalized.as_str() else {
        unreachable!("canonicalized UTF-8 remains UTF-8")
    };
    if normalized_text.is_empty() {
        return Err(DirectoryMountsError::EmptyPrefix);
    }
    if module_name_escapes_root(normalized_text) {
        return Err(DirectoryMountsError::InvalidPrefix {
            prefix: prefix.clone(),
        });
    }
    Ok(normalized)
}

fn prefixes_overlap(first: &ModuleId, second: &ModuleId) -> bool {
    let (Some(first), Some(second)) = (first.as_str(), second.as_str()) else {
        return false;
    };
    first
        .strip_prefix(second)
        .is_some_and(|suffix| suffix.starts_with('/'))
        || second
            .strip_prefix(first)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn path_to_module_name(path: &Path) -> Result<String, DirectoryMountsError> {
    path.iter()
        .map(|component| {
            component.to_str().map(ToOwned::to_owned).ok_or_else(|| {
                DirectoryMountsError::NonUtf8Path {
                    path: path.to_path_buf(),
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("/"))
}

fn prefix_module_id(prefix: &ModuleId, inner: &str) -> ModuleId {
    if inner.is_empty() {
        return prefix.clone();
    }
    ModuleId::new(format!("{}/{inner}", prefix.to_lossy_string()))
}
