//! Filesystem-backed module resolution.

use std::{
    fs,
    path::{Path, PathBuf},
    result::Result as StdResult,
};

use tokio::task::spawn_blocking;

use super::{
    LocalResolveFuture, ModuleId, ModuleResolveError, ModuleResolver, ModuleSource,
    path_util::normalize_path,
};

/// Filesystem resolver for rooted plain Luau path loading.
///
/// This resolver intentionally does not read `.luaurc` or `.config.luau`; applications that need
/// aliases or project configuration should encode that policy in their own [`ModuleResolver`].
/// It resolves `.luau` files only.
///
/// The resolver canonicalizes the configured root and the concrete module file selected after
/// extension probing, then rejects files that do not remain inside the root. Symlinks are followed
/// by canonicalization, so a symlink that points outside the root is rejected. Like any filesystem
/// policy enforced before opening a file, this does not attempt to close every possible TOCTOU race
/// against a hostile filesystem owner.
#[derive(Debug, Clone)]
pub struct FilesystemResolver {
    /// Filesystem root used for non-absolute specifiers.
    root: FilesystemRoot,
}

impl FilesystemResolver {
    /// Creates a filesystem resolver rooted at `root`.
    ///
    /// The root is canonicalized when a module is resolved. Use [`FilesystemResolver::try_new`] to
    /// validate and canonicalize the root immediately.
    #[must_use]
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: FilesystemRoot::deferred(root.as_ref()),
        }
    }

    /// Creates a filesystem resolver and validates the root immediately.
    pub fn try_new(root: impl AsRef<Path>) -> StdResult<Self, ModuleResolveError> {
        Ok(Self {
            root: FilesystemRoot::checked(root.as_ref())?,
        })
    }
}

impl ModuleResolver for FilesystemResolver {
    fn resolve<'a>(
        &'a self,
        requester: Option<&'a ModuleId>,
        specifier: &'a str,
    ) -> LocalResolveFuture<'a> {
        let root = self.root.clone();
        let requester = requester.cloned();
        let specifier = specifier.to_owned();
        Box::pin(async move {
            let module = specifier.clone();
            spawn_blocking(move || resolve_filesystem_source(&root, requester.as_ref(), &specifier))
                .await
                .map_err(|error| ModuleResolveError::Read {
                    module,
                    message: error.to_string(),
                })?
        })
    }
}

/// Resolves a filesystem module specifier and reads the source from disk.
fn resolve_filesystem_source(
    root: &FilesystemRoot,
    requester: Option<&ModuleId>,
    specifier: &str,
) -> StdResult<ModuleSource, ModuleResolveError> {
    let root = root.resolve()?;
    let path = root.resolve_module_path(requester, specifier)?;
    let source = fs::read_to_string(&path).map_err(|error| ModuleResolveError::Read {
        module: specifier.to_owned(),
        message: error.to_string(),
    })?;
    Ok(ModuleSource::with_path(
        ModuleId::from_path(&path),
        source,
        path,
    ))
}

/// Configured resolver root, either checked eagerly or deferred until resolution.
#[derive(Debug, Clone)]
enum FilesystemRoot {
    Deferred(PathBuf),
    Checked(PathBuf),
}

impl FilesystemRoot {
    fn deferred(root: &Path) -> Self {
        Self::Deferred(root.to_path_buf())
    }

    fn checked(root: &Path) -> StdResult<Self, ModuleResolveError> {
        Ok(Self::Checked(ResolvedRoot::canonicalize_path(root)?))
    }

    fn resolve(&self) -> StdResult<ResolvedRoot, ModuleResolveError> {
        match self {
            Self::Deferred(root) => ResolvedRoot::new(root),
            Self::Checked(root) => Ok(ResolvedRoot { path: root.clone() }),
        }
    }
}

/// Canonical resolver root plus the path policy derived from it.
struct ResolvedRoot {
    path: PathBuf,
}

impl ResolvedRoot {
    fn new(root: &Path) -> StdResult<Self, ModuleResolveError> {
        let path = Self::canonicalize_path(root)?;
        Ok(Self { path })
    }

    /// Finds and canonicalizes the module selected by a require specifier.
    fn resolve_module_path(
        &self,
        requester: Option<&ModuleId>,
        specifier: &str,
    ) -> StdResult<PathBuf, ModuleResolveError> {
        let logical = self.logical_path(requester, specifier)?;
        let path = resolve_module_file(&logical).map_err(|error| match error {
            ModuleResolveError::NotFound(_) => ModuleResolveError::NotFound(specifier.to_owned()),
            error => error,
        })?;
        self.canonicalize_child(&path, specifier)
    }

    fn canonicalize_path(root: &Path) -> StdResult<PathBuf, ModuleResolveError> {
        fs::canonicalize(root).map_err(|error| ModuleResolveError::Read {
            module: root.display().to_string(),
            message: error.to_string(),
        })
    }

    /// Converts a require specifier into the logical filesystem path to probe.
    fn logical_path(
        &self,
        requester: Option<&ModuleId>,
        specifier: &str,
    ) -> StdResult<PathBuf, ModuleResolveError> {
        if let Some(self_path) = self_relative_path(specifier) {
            let requester =
                requester.ok_or_else(|| ModuleResolveError::NotFound(specifier.to_owned()))?;
            return Ok(self.requester_base_dir(Some(requester)).join(self_path));
        }

        let candidate = Path::new(specifier);
        if candidate.is_absolute() {
            Ok(candidate.to_path_buf())
        } else {
            Ok(self.requester_base_dir(requester).join(candidate))
        }
    }

    /// Canonicalizes `path` and rejects it if it escapes this root.
    fn canonicalize_child(
        &self,
        path: &Path,
        specifier: &str,
    ) -> StdResult<PathBuf, ModuleResolveError> {
        let canonical = fs::canonicalize(path).map_err(|error| ModuleResolveError::Read {
            module: specifier.to_owned(),
            message: error.to_string(),
        })?;
        if !canonical.starts_with(&self.path) {
            return Err(ModuleResolveError::OutsideRoot {
                specifier: specifier.to_owned(),
            });
        }
        Ok(canonical)
    }

    /// Returns the directory used as the base for requester-relative specifiers.
    fn requester_base_dir(&self, requester: Option<&ModuleId>) -> PathBuf {
        requester
            .and_then(|requester| Path::new(requester.as_str()).parent())
            .map_or_else(
                || self.path.clone(),
                |parent| {
                    if parent.is_absolute() {
                        parent.to_path_buf()
                    } else {
                        self.path.join(parent)
                    }
                },
            )
    }
}

/// Returns the path part of an `@self/...` specifier.
fn self_relative_path(specifier: &str) -> Option<&str> {
    let path = specifier.strip_prefix("@self")?;
    if path.is_empty() {
        Some("")
    } else {
        path.strip_prefix('/')
    }
}

/// Finds a concrete module file using file and `init` extension lookup.
fn resolve_module_file(path: &Path) -> StdResult<PathBuf, ModuleResolveError> {
    let try_luau_path = |candidate: PathBuf| {
        if candidate.is_file() && is_luau_path(&candidate) {
            Some(candidate)
        } else {
            None
        }
    };

    if let Some(found) = try_luau_path(path.to_path_buf()) {
        return Ok(normalize_path(&found));
    }

    if path.file_name() != Some("init".as_ref()) {
        let current_ext = (path.extension().and_then(|s| s.to_str()))
            .map(|s| format!("{s}."))
            .unwrap_or_default();
        if let Some(found) = try_luau_path(path.with_extension(format!("{current_ext}luau"))) {
            return Ok(normalize_path(&found));
        }
    }

    if path.is_dir()
        && let Some(found) = try_luau_path(path.join("init.luau"))
    {
        return Ok(normalize_path(&found));
    }

    Err(ModuleResolveError::NotFound(path.display().to_string()))
}

fn is_luau_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension == "luau")
}
