//! Shared module-resolution contracts for runtime loading and analysis.
#![allow(clippy::missing_docs_in_private_items)]

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt, fs,
    path::{Component, Path, PathBuf},
    result::Result as StdResult,
    slice,
    sync::Arc,
};

use thiserror::Error;

/// Stable module identity used by runtime loading and analysis diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(String);

impl ModuleId {
    /// Creates a module id from a stable label.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the stable module label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Creates a module id from a filesystem path display string.
    #[must_use]
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        Self(path.as_ref().display().to_string())
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for ModuleId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for ModuleId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ModuleId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&Path> for ModuleId {
    fn from(value: &Path) -> Self {
        Self::from_path(value)
    }
}

impl From<PathBuf> for ModuleId {
    fn from(value: PathBuf) -> Self {
        Self::from_path(value)
    }
}

/// Source text and optional filesystem path for one resolved module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSource {
    /// Stable module id.
    id: ModuleId,
    /// Source text.
    source: String,
    /// Filesystem path when this module came from disk.
    path: Option<PathBuf>,
}

impl ModuleSource {
    /// Creates source for a logical module.
    #[must_use]
    pub fn new(id: impl Into<ModuleId>, source: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            path: None,
        }
    }

    /// Creates source for a module read from disk.
    #[must_use]
    pub fn with_path(
        id: impl Into<ModuleId>,
        source: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            path: Some(path.into()),
        }
    }

    /// Returns this module's stable id.
    #[must_use]
    pub fn id(&self) -> &ModuleId {
        &self.id
    }

    /// Returns this module's source text.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the source path when this module came from disk.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// Diagnostic source span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceSpan {
    /// Zero-based start line.
    pub line: u32,
    /// Zero-based start column.
    pub column: u32,
    /// Zero-based end line.
    pub end_line: u32,
    /// Zero-based end column.
    pub end_column: u32,
}

/// Shared module resolver.
pub trait ModuleResolver: Send + Sync + 'static {
    /// Resolves `specifier` from an optional requesting module.
    fn resolve(
        &self,
        requester: Option<&ModuleId>,
        specifier: &str,
    ) -> StdResult<ModuleSource, ModuleResolveError>;
}

impl<T> ModuleResolver for Arc<T>
where
    T: ModuleResolver + ?Sized,
{
    fn resolve(
        &self,
        requester: Option<&ModuleId>,
        specifier: &str,
    ) -> StdResult<ModuleSource, ModuleResolveError> {
        (**self).resolve(requester, specifier)
    }
}

/// Module resolution failure.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ModuleResolveError {
    /// The requested module was not found.
    #[error("module not found: {0}")]
    NotFound(String),
    /// The requested module path is ambiguous.
    #[error("module is ambiguous: {0}")]
    Ambiguous(String),
    /// The module could not be read.
    #[error("failed to read {module}: {message}")]
    Read {
        /// Module label or path.
        module: String,
        /// Human-readable read error.
        message: String,
    },
    /// The module could not be parsed for dependency discovery.
    #[error("failed to parse {module}: {message}")]
    Parse {
        /// Module label or path.
        module: String,
        /// Human-readable parse error.
        message: String,
    },
}

/// Immutable resolved graph used by checked loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverSnapshot {
    /// Root module id.
    root: ModuleId,
    /// Resolved modules keyed by id.
    modules: BTreeMap<ModuleId, ModuleSource>,
    /// Resolved dependency edges keyed by requesting module and original specifier.
    edges: BTreeMap<ModuleId, BTreeMap<String, ModuleId>>,
}

impl ResolverSnapshot {
    /// Resolves a root module and its direct string-literal dependencies.
    pub fn resolve<R: ModuleResolver>(
        resolver: &R,
        root: impl Into<ModuleId>,
    ) -> StdResult<Self, ModuleResolveError> {
        let root = resolver.resolve(None, root.into().as_str())?;
        let root_id = root.id.clone();
        let mut modules = BTreeMap::new();
        modules.insert(root.id.clone(), root);
        let mut edges = BTreeMap::new();
        let mut queue = VecDeque::from([root_id.clone()]);
        let mut queued = HashSet::from([root_id.clone()]);

        while let Some(id) = queue.pop_front() {
            let (source_id, requires) = {
                let source = modules
                    .get(&id)
                    .expect("queued resolver snapshot module is missing");
                (
                    source.id.clone(),
                    require_specifiers(source.id(), source.source())?,
                )
            };
            for required in requires {
                let dep = resolver.resolve(Some(&source_id), &required.specifier)?;
                edges
                    .entry(source_id.clone())
                    .or_insert_with(BTreeMap::new)
                    .insert(required.specifier, dep.id.clone());
                if queued.insert(dep.id.clone()) {
                    let dep_id = dep.id.clone();
                    modules.insert(dep.id.clone(), dep);
                    queue.push_back(dep_id);
                }
            }
        }

        Ok(Self {
            root: root_id,
            modules,
            edges,
        })
    }

    /// Returns the root module id.
    #[must_use]
    pub fn root(&self) -> &ModuleId {
        &self.root
    }

    /// Returns the root module source.
    #[must_use]
    pub fn root_source(&self) -> Option<&ModuleSource> {
        self.modules.get(&self.root)
    }

    /// Returns all resolved module sources in stable id order.
    pub fn modules(&self) -> impl Iterator<Item = &ModuleSource> {
        self.modules.values()
    }

    /// Returns the module source for `specifier` as resolved from `requester`.
    #[must_use]
    pub fn dependency(&self, requester: &ModuleId, specifier: &str) -> Option<&ModuleSource> {
        self.edges
            .get(requester)
            .and_then(|edges| edges.get(specifier))
            .and_then(|id| self.modules.get(id))
            .or_else(|| self.modules.get(&ModuleId::new(specifier)))
    }

    /// Returns non-root modules as analyzer virtual modules.
    #[must_use]
    pub fn virtual_modules(&self) -> Vec<crate::analyzer::VirtualModule<'_>> {
        self.modules
            .iter()
            .filter(|(id, _)| **id != self.root)
            .map(|(id, module)| crate::analyzer::VirtualModule {
                name: id.as_str(),
                source: module.source(),
            })
            .collect()
    }
}

/// In-memory resolver for tests and embedders.
#[derive(Debug, Clone, Default)]
pub struct InMemoryResolver {
    modules: HashMap<ModuleId, String>,
}

impl InMemoryResolver {
    /// Creates an empty in-memory resolver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a module.
    #[must_use]
    pub fn with_module(mut self, id: impl Into<ModuleId>, source: impl Into<String>) -> Self {
        self.insert_module(id, source);
        self
    }

    /// Adds or replaces a module.
    pub fn insert_module(
        &mut self,
        id: impl Into<ModuleId>,
        source: impl Into<String>,
    ) -> Option<String> {
        self.modules.insert(id.into(), source.into())
    }
}

impl ModuleResolver for InMemoryResolver {
    fn resolve(
        &self,
        requester: Option<&ModuleId>,
        specifier: &str,
    ) -> StdResult<ModuleSource, ModuleResolveError> {
        let id = if specifier.starts_with("./") || specifier.starts_with("../") {
            let requester =
                requester.ok_or_else(|| ModuleResolveError::NotFound(specifier.into()))?;
            let parent = Path::new(requester.as_str())
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let path = parent.join(specifier);
            ModuleId::new(normalize_path(&path).display().to_string())
        } else {
            ModuleId::new(specifier)
        };
        let source = self
            .modules
            .get(&id)
            .ok_or_else(|| ModuleResolveError::NotFound(id.as_str().to_owned()))?;
        Ok(ModuleSource::new(id, source.clone()))
    }
}

/// Filesystem resolver for plain Luau path loading.
///
/// This resolver intentionally does not read `.luaurc` or `.config.luau`; applications that need
/// aliases or project configuration should encode that policy in their own [`ModuleResolver`].
/// It keeps both `.luau` and `.lua` module lookup for parity with Luau tooling.
#[derive(Debug, Clone)]
pub struct FilesystemResolver {
    root: PathBuf,
}

impl FilesystemResolver {
    /// Creates a filesystem resolver rooted at `root`.
    #[must_use]
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
}

impl ModuleResolver for FilesystemResolver {
    fn resolve(
        &self,
        requester: Option<&ModuleId>,
        specifier: &str,
    ) -> StdResult<ModuleSource, ModuleResolveError> {
        let base = if let Some(requester) = requester {
            Path::new(requester.as_str())
                .parent()
                .map_or_else(|| self.root.clone(), Path::to_path_buf)
        } else {
            self.root.clone()
        };
        let logical = if specifier == "@self" || specifier.starts_with("@self/") {
            let self_path = specifier.strip_prefix("@self").expect("checked @self prefix");
            let requester = requester
                .ok_or_else(|| ModuleResolveError::NotFound(specifier.to_owned()))?;
            let requester_path = Path::new(requester.as_str());
            let base = requester_path
                .parent()
                .map_or_else(|| self.root.clone(), Path::to_path_buf);
            base.join(self_path.strip_prefix('/').unwrap_or(self_path))
        } else {
            let candidate = Path::new(specifier);
            if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                base.join(candidate)
            }
        };
        let path = resolve_module_file(&logical)?;
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
}

fn resolve_module_file(path: &Path) -> StdResult<PathBuf, ModuleResolveError> {
    let mut found = None;
    let mut try_path = |candidate: PathBuf| {
        if candidate.is_file() && found.replace(candidate).is_some() {
            return Err(ModuleResolveError::Ambiguous(path.display().to_string()));
        }
        Ok(())
    };

    if path.file_name() != Some("init".as_ref()) {
        let current_ext = (path.extension().and_then(|s| s.to_str()))
            .map(|s| format!("{s}."))
            .unwrap_or_default();
        for ext in ["luau", "lua"] {
            try_path(path.with_extension(format!("{current_ext}{ext}")))?;
        }
    }

    if path.is_dir() {
        for ext in ["luau", "lua"] {
            try_path(path.join(format!("init.{ext}")))?;
        }
    }

    found
        .map(|path| normalize_path(&path))
        .ok_or_else(|| ModuleResolveError::NotFound(path.display().to_string()))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut components = VecDeque::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(..) | Component::RootDir => components.push_back(comp),
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(components.back(), None | Some(Component::ParentDir)) {
                    components.push_back(Component::ParentDir);
                } else if matches!(components.back(), Some(Component::Normal(..))) {
                    components.pop_back();
                }
            }
            Component::Normal(..) => components.push_back(comp),
        }
    }
    components.into_iter().collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequireSpecifier {
    specifier: String,
    _span: SourceSpan,
}

fn require_specifiers(
    module: &ModuleId,
    source: &str,
) -> StdResult<Vec<RequireSpecifier>, ModuleResolveError> {
    let source_len = u32::try_from(source.len()).map_err(|_| ModuleResolveError::Parse {
        module: module.to_string(),
        message: format!("source is too large: {} bytes", source.len()),
    })?;
    let raw = unsafe { ffi::ruau_trace_requires(source.as_ptr(), source_len) };
    let guard = RequireTraceGuard(raw);
    if raw.error_len != 0 {
        return Err(ModuleResolveError::Parse {
            module: module.to_string(),
            message: unsafe { string_from_raw(raw.error, raw.error_len) },
        });
    }

    if raw.specifier_count == 0 {
        return Ok(Vec::new());
    }

    let rows = unsafe { slice::from_raw_parts(raw.specifiers, raw.specifier_count as usize) };
    let specifiers = rows
        .iter()
        .map(|row| {
            Ok(RequireSpecifier {
                specifier: unsafe { string_from_raw(row.specifier, row.specifier_len) },
                _span: SourceSpan {
                    line: row.line,
                    column: row.col,
                    end_line: row.end_line,
                    end_column: row.end_col,
                },
            })
        })
        .collect::<StdResult<Vec<_>, ModuleResolveError>>();
    drop(guard);
    specifiers
}

struct RequireTraceGuard(ffi::RuauRequireTraceResult);

impl Drop for RequireTraceGuard {
    fn drop(&mut self) {
        unsafe { ffi::ruau_require_trace_result_free(self.0) };
    }
}

unsafe fn string_from_raw(data: *const u8, len: u32) -> String {
    String::from_utf8_lossy(slice::from_raw_parts(data, len as usize)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{InMemoryResolver, ModuleId, ResolverSnapshot, require_specifiers};

    #[test]
    fn require_specifiers_ignores_comments_and_strings() {
        let source = r#"
-- require('commented')
--[[ require('block_comment') ]]
--[=[ require('equals_block_comment') ]=]
local text = "require('text')"
local escaped = 'require("also_text")'
local long = [[ require('long_text') ]]
local equals_long = [=[ require('equals_long_text') ]=]
return require('dep')
"#;

        let requires = require_specifiers(&ModuleId::new("main"), source).expect("requires");
        assert_eq!(
            vec!["dep"],
            requires.into_iter().map(|r| r.specifier).collect::<Vec<_>>()
        );
    }

    #[test]
    fn require_specifiers_accepts_whitespace_before_literal() {
        let requires =
            require_specifiers(&ModuleId::new("main"), r#"return require ( "dep" )"#).expect("requires");
        assert_eq!(
            vec!["dep"],
            requires.into_iter().map(|r| r.specifier).collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolver_snapshot_discovers_only_real_requires() {
        let resolver = InMemoryResolver::new()
            .with_module(
                "main",
                r#"
-- require('missing_comment')
local text = "require('missing_string')"
return require ( 'dep' )
"#,
            )
            .with_module("dep", "return { value = 7 }");

        let snapshot = ResolverSnapshot::resolve(&resolver, "main").expect("snapshot");

        assert_eq!(2, snapshot.modules().count());
        assert!(snapshot.dependency(&ModuleId::new("main"), "dep").is_some());
    }
}
