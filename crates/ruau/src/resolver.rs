//! Shared module-resolution contracts for runtime loading and analysis.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt, fs,
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    rc::Rc,
    result::Result as StdResult,
    slice,
};

use thiserror::Error;
use tokio::task::spawn_blocking;

use crate::analyzer::VirtualModule;

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

/// Kind of source represented by a resolved module record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleSourceKind {
    /// Executable Luau source that can be loaded at runtime.
    #[default]
    Executable,
    /// Interface-only declaration source used for analysis and documentation.
    Interface,
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
    /// Whether this record is executable source or an interface declaration.
    kind: ModuleSourceKind,
}

impl ModuleSource {
    /// Creates source for a logical module.
    #[must_use]
    pub fn new(id: impl Into<ModuleId>, source: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            path: None,
            kind: ModuleSourceKind::Executable,
        }
    }

    /// Creates source for a logical interface-only module.
    #[must_use]
    pub fn interface(id: impl Into<ModuleId>, source: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            path: None,
            kind: ModuleSourceKind::Interface,
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
            kind: ModuleSourceKind::Executable,
        }
    }

    /// Returns a copy of this source with a different source kind.
    #[must_use]
    pub fn with_kind(mut self, kind: ModuleSourceKind) -> Self {
        self.kind = kind;
        self
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

    /// Returns whether this source is executable or interface-only.
    #[must_use]
    pub const fn kind(&self) -> ModuleSourceKind {
        self.kind
    }

    /// Returns true if this source can be loaded at runtime.
    #[must_use]
    pub const fn is_executable(&self) -> bool {
        matches!(self.kind, ModuleSourceKind::Executable)
    }

    /// Returns true if this source is an interface declaration.
    #[must_use]
    pub const fn is_interface(&self) -> bool {
        matches!(self.kind, ModuleSourceKind::Interface)
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

/// Local resolver future returned by [`ModuleResolver`].
///
/// Resolver futures are intentionally not `Send`: they run on the same local VM lane as the Luau
/// state and may close over `Rc` or other thread-affine embedder state.
pub type LocalResolveFuture<'a> =
    Pin<Box<dyn Future<Output = StdResult<ModuleSource, ModuleResolveError>> + 'a>>;

/// Shared module resolver.
///
/// Resolvers run on the same `!Send` thread as the Luau VM, so they can close over `!Send` data
/// (caches, channels, etc.). The returned future is similarly `!Send` so the trait stays
/// dyn-compatible through `Rc<dyn ModuleResolver>`.
pub trait ModuleResolver: 'static {
    /// Resolves `specifier` from an optional requesting module.
    fn resolve<'a>(
        &'a self,
        requester: Option<&'a ModuleId>,
        specifier: &'a str,
    ) -> LocalResolveFuture<'a>;
}

impl<T> ModuleResolver for Rc<T>
where
    T: ModuleResolver + ?Sized,
{
    fn resolve<'a>(
        &'a self,
        requester: Option<&'a ModuleId>,
        specifier: &'a str,
    ) -> LocalResolveFuture<'a> {
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
///
/// Snapshot resolution walks `require(...)` dependencies through executable modules only.
/// [`ModuleSourceKind::Interface`] entries are retained in the snapshot when directly resolved,
/// but their source is not traversed for further dependencies because declaration modules have no
/// runtime body.
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
    pub async fn resolve<R: ModuleResolver + ?Sized>(
        resolver: &R,
        root: impl Into<ModuleId>,
    ) -> StdResult<Self, ModuleResolveError> {
        let root = resolver.resolve(None, root.into().as_str()).await?;
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
                if source.is_interface() {
                    continue;
                }
                (
                    source.id.clone(),
                    require_specifiers(source.id(), source.source())?,
                )
            };
            for required in requires {
                let dep = resolver
                    .resolve(Some(&source_id), &required.specifier)
                    .await?;
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
    pub fn virtual_modules(&self) -> Vec<VirtualModule<'_>> {
        self.modules
            .iter()
            .filter(|(id, _)| **id != self.root)
            .map(|(id, module)| VirtualModule {
                name: id.as_str(),
                source: module.source(),
            })
            .collect()
    }
}

impl ModuleResolver for ResolverSnapshot {
    fn resolve<'a>(
        &'a self,
        requester: Option<&'a ModuleId>,
        specifier: &'a str,
    ) -> LocalResolveFuture<'a> {
        Box::pin(async move {
            // The snapshot was already produced by walking a real resolver, so a missing entry
            // is a resolution error here too.
            let module = match requester {
                Some(req) => self.dependency(req, specifier),
                None => self.modules.get(&ModuleId::new(specifier)),
            }
            .ok_or_else(|| ModuleResolveError::NotFound(specifier.to_owned()))?;
            Ok(module.clone())
        })
    }
}

/// In-memory resolver for tests and embedders.
#[derive(Debug, Clone, Default)]
pub struct InMemoryResolver {
    /// Source text keyed by stable module id.
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
    fn resolve<'a>(
        &'a self,
        requester: Option<&'a ModuleId>,
        specifier: &'a str,
    ) -> LocalResolveFuture<'a> {
        Box::pin(async move {
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
        })
    }
}

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
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        Ok(None)
    };

    if path.file_name() != Some("init".as_ref()) {
        let current_ext = (path.extension().and_then(|s| s.to_str()))
            .map(|s| format!("{s}."))
            .unwrap_or_default();
        for ext in extensions {
            if let Some(found) = try_path(path.with_extension(format!("{current_ext}{ext}")))? {
                return Ok(normalize_path(&found));
            }
        }
    }

    if path.is_dir() {
        for ext in extensions {
            if let Some(found) = try_path(path.join(format!("init.{ext}")))? {
                return Ok(normalize_path(&found));
            }
        }
    }

    Err(ModuleResolveError::NotFound(path.display().to_string()))
}

/// Normalizes `.` and `..` path components without touching the filesystem.
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
/// Literal require specifier plus source span returned by Luau tracing.
struct RequireSpecifier {
    /// Required module specifier.
    specifier: String,
    /// Location of the literal specifier in source.
    _span: SourceSpan,
}

/// Returns literal require specifiers with source spans.
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

/// Returns literal `require(...)` specifiers discovered in `source`.
///
/// Comments, strings, and dynamic require expressions are ignored. The returned
/// specifiers are in source order and are not resolved relative to `module`.
pub fn required_specifiers(
    module: &ModuleId,
    source: &str,
) -> StdResult<Vec<String>, ModuleResolveError> {
    require_specifiers(module, source).map(|specifiers| {
        specifiers
            .into_iter()
            .map(|specifier| specifier.specifier)
            .collect()
    })
}

/// Frees a raw Luau require tracing result on drop.
struct RequireTraceGuard(ffi::RuauRequireTraceResult);

impl Drop for RequireTraceGuard {
    fn drop(&mut self) {
        unsafe { ffi::ruau_require_trace_result_free(self.0) };
    }
}

/// Converts a raw UTF-8-ish byte range from Luau tracing into an owned string.
unsafe fn string_from_raw(data: *const u8, len: u32) -> String {
    String::from_utf8_lossy(slice::from_raw_parts(data, len as usize)).into_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        FilesystemResolver, InMemoryResolver, ModuleId, ModuleResolveError, ModuleResolver,
        ResolverSnapshot, require_specifiers, required_specifiers,
    };

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
            requires
                .into_iter()
                .map(|r| r.specifier)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn require_specifiers_accepts_whitespace_before_literal() {
        let requires = require_specifiers(&ModuleId::new("main"), r#"return require ( "dep" )"#)
            .expect("requires");
        assert_eq!(
            vec!["dep"],
            requires
                .into_iter()
                .map(|r| r.specifier)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn required_specifiers_returns_public_string_list() {
        let requires =
            required_specifiers(&ModuleId::new("main"), "return require('dep')").expect("requires");
        assert_eq!(requires, vec!["dep"]);
    }

    #[tokio::test]
    async fn resolver_snapshot_discovers_only_real_requires() {
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

        let snapshot = ResolverSnapshot::resolve(&resolver, "main")
            .await
            .expect("snapshot");

        assert_eq!(2, snapshot.modules().count());
        assert!(snapshot.dependency(&ModuleId::new("main"), "dep").is_some());
    }

    #[tokio::test]
    async fn filesystem_resolver_uses_luau_extension_by_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("main.luau"), "return 1").expect("write main");

        let source = FilesystemResolver::new(dir.path())
            .resolve(None, "main")
            .await
            .expect("resolve");

        assert_eq!(source.source(), "return 1");
        assert!(
            source
                .path()
                .is_some_and(|path| path.ends_with("main.luau"))
        );
    }

    #[tokio::test]
    async fn filesystem_resolver_does_not_load_lua_by_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("main.lua"), "return 1").expect("write main");

        let err = FilesystemResolver::new(dir.path())
            .resolve(None, "main")
            .await
            .expect_err("default resolver should ignore .lua");

        assert!(matches!(err, ModuleResolveError::NotFound(_)));
    }

    #[tokio::test]
    async fn filesystem_resolver_extension_override_uses_ordered_precedence() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("main.luau"), "return 'luau'").expect("write luau");
        fs::write(dir.path().join("main.lua"), "return 'lua'").expect("write lua");

        let source = FilesystemResolver::new(dir.path())
            .with_extensions(["lua", "luau"])
            .resolve(None, "main")
            .await
            .expect("resolve");

        assert_eq!(source.source(), "return 'lua'");
        assert!(source.path().is_some_and(|path| path.ends_with("main.lua")));
    }

    #[tokio::test]
    async fn filesystem_resolver_resolves_init_luau_directory_modules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package = dir.path().join("package");
        fs::create_dir(&package).expect("create package");
        fs::write(package.join("init.luau"), "return 'package'").expect("write init");

        let source = FilesystemResolver::new(dir.path())
            .resolve(None, "package")
            .await
            .expect("resolve");

        assert_eq!(source.source(), "return 'package'");
        assert!(
            source
                .path()
                .is_some_and(|path| path.ends_with("init.luau"))
        );
    }

    #[tokio::test]
    async fn filesystem_resolver_resolves_self_relative_to_requester() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        fs::create_dir(&src).expect("create src");
        let requester = src.join("main.luau");
        fs::write(&requester, "return require('@self/dep')").expect("write main");
        fs::write(src.join("dep.luau"), "return 'dep'").expect("write dep");

        let source = FilesystemResolver::new(dir.path())
            .resolve(Some(&ModuleId::from_path(&requester)), "@self/dep")
            .await
            .expect("resolve");

        assert_eq!(source.source(), "return 'dep'");
        assert!(source.path().is_some_and(|path| path.ends_with("dep.luau")));
    }

    #[tokio::test]
    async fn filesystem_resolver_rejects_self_without_requester() {
        let dir = tempfile::tempdir().expect("tempdir");

        let err = FilesystemResolver::new(dir.path())
            .resolve(None, "@self/dep")
            .await
            .expect_err("@self requires a requester");

        assert_eq!(err, ModuleResolveError::NotFound("@self/dep".to_owned()));
    }

    #[tokio::test]
    async fn filesystem_resolver_does_not_treat_self_prefix_as_alias() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("@selfish.luau"), "return 'plain module'").expect("write module");

        let source = FilesystemResolver::new(dir.path())
            .resolve(None, "@selfish")
            .await
            .expect("resolve");

        assert_eq!(source.source(), "return 'plain module'");
    }
}
