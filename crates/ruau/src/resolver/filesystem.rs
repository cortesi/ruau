//! Filesystem-backed module resolution.

use std::{
    fs,
    path::{Path, PathBuf},
    result::Result as StdResult,
};

use tokio::task::spawn_blocking;

use super::{
    LocalResolveFuture, ModuleId, ModuleResolveError, ModuleResolver, ModuleSource, normalize_path,
};

/// Filesystem resolver for plain Luau path loading.
///
/// This resolver intentionally does not read `.luaurc` or `.config.luau`; applications that need
/// aliases or project configuration should encode that policy in their own [`ModuleResolver`].
/// It resolves `.luau` files by default. Use [`FilesystemResolver::with_extensions`] when a
/// project intentionally stores Luau source under another extension.
#[derive(Debug, Clone)]
pub struct FilesystemResolver {
    /// Filesystem root used for non-absolute specifiers.
    root: PathBuf,
    /// Extension lookup order without leading dots.
    extensions: Vec<String>,
}

impl FilesystemResolver {
    /// Creates a filesystem resolver rooted at `root`.
    #[must_use]
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            extensions: vec!["luau".to_owned()],
        }
    }

    /// Sets the extension lookup order for modules.
    ///
    /// Extensions are tried in the provided order, so
    /// `with_extensions(["luau", "lua"])` resolves `foo.luau` before `foo.lua`.
    /// Extensions may be passed with or without a leading dot.
    #[must_use]
    pub fn with_extensions<I, S>(mut self, extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.extensions = extensions
            .into_iter()
            .map(|ext| ext.as_ref().trim_start_matches('.').to_owned())
            .filter(|ext| !ext.is_empty())
            .collect();
        self
    }
}

impl ModuleResolver for FilesystemResolver {
    fn resolve<'a>(
        &'a self,
        requester: Option<&'a ModuleId>,
        specifier: &'a str,
    ) -> LocalResolveFuture<'a> {
        let root = self.root.clone();
        let extensions = self.extensions.clone();
        let requester = requester.cloned();
        let specifier = specifier.to_owned();
        Box::pin(async move {
            let module = specifier.clone();
            spawn_blocking(move || {
                resolve_filesystem_source(&root, &extensions, requester.as_ref(), &specifier)
            })
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
    root: &Path,
    extensions: &[String],
    requester: Option<&ModuleId>,
    specifier: &str,
) -> StdResult<ModuleSource, ModuleResolveError> {
    let logical = logical_filesystem_path(root, requester, specifier)?;
    let path = resolve_module_file(&logical, extensions)?;
    let source = fs::read_to_string(&path).map_err(|error| ModuleResolveError::Read {
        module: path.display().to_string(),
        message: error.to_string(),
    })?;
    Ok(ModuleSource::with_path(
        ModuleId::from_path(&path),
        source,
        path,
    ))
}

/// Converts a require specifier into the logical filesystem path to probe.
fn logical_filesystem_path(
    root: &Path,
    requester: Option<&ModuleId>,
    specifier: &str,
) -> StdResult<PathBuf, ModuleResolveError> {
    if let Some(self_path) = self_relative_path(specifier) {
        let requester =
            requester.ok_or_else(|| ModuleResolveError::NotFound(specifier.to_owned()))?;
        return Ok(requester_base_dir(root, Some(requester)).join(self_path));
    }

    let candidate = Path::new(specifier);
    if candidate.is_absolute() {
        Ok(candidate.to_path_buf())
    } else {
        Ok(requester_base_dir(root, requester).join(candidate))
    }
}

/// Returns the directory used as the base for requester-relative specifiers.
fn requester_base_dir(root: &Path, requester: Option<&ModuleId>) -> PathBuf {
    requester
        .and_then(|requester| Path::new(requester.as_str()).parent())
        .map_or_else(|| root.to_path_buf(), Path::to_path_buf)
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
fn resolve_module_file(
    path: &Path,
    extensions: &[String],
) -> StdResult<PathBuf, ModuleResolveError> {
    let try_path = |candidate: PathBuf| {
        if candidate.is_file() && has_allowed_extension(&candidate, extensions) {
            Some(candidate)
        } else {
            None
        }
    };

    if let Some(found) = try_path(path.to_path_buf()) {
        return Ok(normalize_path(&found));
    }

    if path.file_name() != Some("init".as_ref()) {
        let current_ext = (path.extension().and_then(|s| s.to_str()))
            .map(|s| format!("{s}."))
            .unwrap_or_default();
        for ext in extensions {
            if let Some(found) = try_path(path.with_extension(format!("{current_ext}{ext}"))) {
                return Ok(normalize_path(&found));
            }
        }
    }

    if path.is_dir() {
        for ext in extensions {
            if let Some(found) = try_path(path.join(format!("init.{ext}"))) {
                return Ok(normalize_path(&found));
            }
        }
    }

    Err(ModuleResolveError::NotFound(path.display().to_string()))
}

/// Returns true if `path` has an extension configured for module loading.
fn has_allowed_extension(path: &Path, extensions: &[String]) -> bool {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return false;
    };
    extensions.iter().any(|allowed| allowed == extension)
}
