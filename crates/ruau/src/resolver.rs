//! Shared module-resolution contracts for runtime loading and analysis.
//!
//! Resolver snapshots use a conservative static dependency policy: only direct string-literal calls
//! shaped like `require("module")` or `require('module')` are walked while building a
//! [`ResolverSnapshot`]. Comments, strings, and dynamic expressions such as `require(name)` are
//! ignored by snapshot resolution. Checked loading also asks Luau's analyzer to validate require
//! calls, so unsupported dynamic require expressions are rejected during analysis instead of being
//! added to the runtime snapshot.
//!
//! Interface-only declarations are not runtime modules. If a resolver returns
//! [`ModuleSourceKind::Interface`] while building a snapshot, resolution fails with
//! [`ModuleResolveError::NotExecutable`]; register declaration-only APIs through
//! [`crate::analyzer::ModuleInterfaceSet`] instead.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    rc::Rc,
    result::Result as StdResult,
};

use thiserror::Error;

mod filesystem;
mod require_spec;
mod snapshot;

pub use filesystem::FilesystemResolver;
use require_spec::require_specifiers;
pub use require_spec::{RequireSpecifier, required_specifiers, required_specifiers_with_spans};
pub use snapshot::{RequireEdge, ResolverSnapshot};

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
    /// The resolver returned an interface-only module where runtime-loadable source is required.
    #[error(
        "module is not executable: {0}; register declaration-only modules with ModuleInterfaceSet"
    )]
    NotExecutable(String),
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

    /// Builder-style insertion of a module by id.
    ///
    /// Replaces any module previously registered under the same id and discards the prior source.
    /// Use [`InMemoryResolver::insert_module`] when the prior source is needed or when working
    /// with a long-lived resolver after construction.
    #[must_use]
    pub fn with_module(mut self, id: impl Into<ModuleId>, source: impl Into<String>) -> Self {
        self.insert_module(id, source);
        self
    }

    /// Inserts a module by id, returning the previous source for that id if one was registered.
    ///
    /// Mirrors [`std::collections::HashMap::insert`]. Use [`InMemoryResolver::with_module`] for
    /// builder-style construction where the previous value is not needed.
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        FilesystemResolver, InMemoryResolver, LocalResolveFuture, ModuleId, ModuleResolveError,
        ModuleResolver, ModuleSource, ResolverSnapshot, require_specifiers, required_specifiers,
        required_specifiers_with_spans,
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

    #[test]
    fn required_specifiers_ignores_dynamic_require_calls() {
        let requires = required_specifiers(
            &ModuleId::new("main"),
            r#"
local name = "dep"
local dynamic = require(name)
local literal = require("literal")
return dynamic, literal
"#,
        )
        .expect("requires");
        assert_eq!(requires, vec!["literal"]);
    }

    #[test]
    fn required_specifiers_with_spans_keeps_locations() {
        let requires =
            required_specifiers_with_spans(&ModuleId::new("main"), "return require('dep')")
                .expect("requires");
        assert_eq!(requires.len(), 1);
        assert_eq!(requires[0].specifier, "dep");
        assert_eq!(requires[0].span.line, 0);
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
        let edges = snapshot.require_edges().collect::<Vec<_>>();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].requester.as_str(), "main");
        assert_eq!(edges[0].specifier, "dep");
        assert_eq!(edges[0].dependency.as_str(), "dep");
    }

    struct InterfaceResolver;

    impl ModuleResolver for InterfaceResolver {
        fn resolve<'a>(
            &'a self,
            _requester: Option<&'a ModuleId>,
            specifier: &'a str,
        ) -> LocalResolveFuture<'a> {
            Box::pin(async move {
                match specifier {
                    "main" => Ok(ModuleSource::new("main", "return require('iface')")),
                    "iface" => Ok(ModuleSource::interface(
                        "iface",
                        "export type Module = { value: number }",
                    )),
                    other => Err(ModuleResolveError::NotFound(other.to_owned())),
                }
            })
        }
    }

    #[tokio::test]
    async fn resolver_snapshot_rejects_interface_dependencies() {
        let err = ResolverSnapshot::resolve(&InterfaceResolver, "main")
            .await
            .expect_err("interface dependencies are not runtime-loadable");

        assert_eq!(err, ModuleResolveError::NotExecutable("iface".to_owned()));
        assert!(err.to_string().contains("ModuleInterfaceSet"));
    }

    #[tokio::test]
    async fn resolver_snapshot_rejects_interface_roots() {
        let err = ResolverSnapshot::resolve(&InterfaceResolver, "iface")
            .await
            .expect_err("interface roots are not runtime-loadable");

        assert_eq!(err, ModuleResolveError::NotExecutable("iface".to_owned()));
        assert!(err.to_string().contains("ModuleInterfaceSet"));
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
    async fn filesystem_resolver_accepts_explicit_file_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("main.luau"), "return 1").expect("write main");

        let source = FilesystemResolver::new(dir.path())
            .resolve(None, "main.luau")
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
    async fn filesystem_resolver_accepts_explicit_init_file_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package = dir.path().join("package");
        fs::create_dir(&package).expect("create package");
        fs::write(package.join("init.luau"), "return 'package'").expect("write init");

        let source = FilesystemResolver::new(dir.path())
            .resolve(None, "package/init.luau")
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
    async fn filesystem_resolver_rejects_explicit_disallowed_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("main.lua"), "return 1").expect("write main");

        let err = FilesystemResolver::new(dir.path())
            .resolve(None, "main.lua")
            .await
            .expect_err("default resolver should reject explicit .lua files");

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
