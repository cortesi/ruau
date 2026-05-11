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
    fmt,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    result::Result as StdResult,
};

use thiserror::Error;

mod filesystem;
mod in_memory;
mod path_util;
mod require_spec;
mod snapshot;

pub use filesystem::FilesystemResolver;
pub use in_memory::InMemoryResolver;
#[cfg(test)]
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
    /// The requested filesystem module resolved outside the configured resolver root.
    #[error("module outside resolver root: {specifier}")]
    OutsideRoot {
        /// Require specifier that resolved outside the configured root.
        specifier: String,
    },
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::symlink as symlink_file;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file;
    use std::{fs, io, path::Path};

    use super::{
        FilesystemResolver, InMemoryResolver, LocalResolveFuture, ModuleId, ModuleResolveError,
        ModuleResolver, ModuleSource, ResolverSnapshot, require_specifiers, required_specifiers,
        required_specifiers_with_spans,
    };

    fn assert_filesystem_source(source: &ModuleSource, expected_source: &str, expected_file: &str) {
        assert_eq!(source.source(), expected_source);
        assert!(
            source
                .path()
                .is_some_and(|path| path.ends_with(expected_file)),
            "expected source path to end with {expected_file:?}, got {:?}",
            source.path()
        );
    }

    fn assert_outside_root(err: ModuleResolveError, specifier: &str) {
        assert_eq!(
            err,
            ModuleResolveError::OutsideRoot {
                specifier: specifier.to_owned()
            }
        );
    }

    fn assert_diagnostic_hides_path(err: &ModuleResolveError, hidden: &Path) {
        assert!(
            !err.to_string().contains(hidden.to_string_lossy().as_ref()),
            "diagnostic leaked hidden path {hidden:?}: {err}"
        );
    }

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

    #[test]
    fn filesystem_resolver_try_new_rejects_missing_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing");

        let err = FilesystemResolver::try_new(&missing).expect_err("missing root");

        assert!(matches!(
            err,
            ModuleResolveError::Read { module, .. } if module == missing.display().to_string()
        ));
    }

    #[tokio::test]
    async fn filesystem_resolver_try_new_resolves_from_checked_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("main.luau"), "return 'checked'").expect("write main");

        let source = FilesystemResolver::try_new(dir.path())
            .expect("checked root")
            .resolve(None, "main")
            .await
            .expect("resolve");

        assert_filesystem_source(&source, "return 'checked'", "main.luau");
    }

    #[tokio::test]
    async fn filesystem_resolver_uses_luau_extension_by_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("main.luau"), "return 1").expect("write main");

        let source = FilesystemResolver::new(dir.path())
            .resolve(None, "main")
            .await
            .expect("resolve");

        assert_filesystem_source(&source, "return 1", "main.luau");
    }

    #[tokio::test]
    async fn filesystem_resolver_accepts_explicit_file_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("main.luau"), "return 1").expect("write main");

        let source = FilesystemResolver::new(dir.path())
            .resolve(None, "main.luau")
            .await
            .expect("resolve");

        assert_filesystem_source(&source, "return 1", "main.luau");
    }

    #[tokio::test]
    async fn filesystem_resolver_accepts_absolute_path_under_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("main.luau");
        fs::write(&path, "return 1").expect("write main");

        let source = FilesystemResolver::new(dir.path())
            .resolve(None, path.to_str().expect("utf8 path"))
            .await
            .expect("resolve");

        assert_filesystem_source(&source, "return 1", "main.luau");
    }

    #[tokio::test]
    async fn filesystem_resolver_rejects_absolute_path_outside_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let path = outside.path().join("outside.luau");
        fs::write(&path, "return 'outside'").expect("write outside");
        let specifier = path.to_str().expect("utf8 path");

        let err = FilesystemResolver::new(dir.path())
            .resolve(None, specifier)
            .await
            .expect_err("absolute path outside root");

        assert_outside_root(err, specifier);
    }

    #[tokio::test]
    async fn filesystem_resolver_rejects_parent_traversal_escape() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        fs::create_dir(&root).expect("create root");
        fs::write(base.path().join("outside.luau"), "return 'outside'").expect("write outside");

        let err = FilesystemResolver::new(&root)
            .resolve(None, "../outside")
            .await
            .expect_err("parent traversal outside root");

        assert_outside_root(err.clone(), "../outside");
        assert_diagnostic_hides_path(&err, base.path());
    }

    #[tokio::test]
    async fn filesystem_resolver_rejects_self_parent_traversal_escape() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        let src = root.join("src");
        fs::create_dir_all(&src).expect("create src");
        let requester = src.join("main.luau");
        fs::write(&requester, "return require('@self/../../outside')").expect("write main");
        fs::write(base.path().join("outside.luau"), "return 'outside'").expect("write outside");

        let err = FilesystemResolver::new(&root)
            .resolve(
                Some(&ModuleId::from_path(&requester)),
                "@self/../../outside",
            )
            .await
            .expect_err("@self traversal outside root");

        assert_outside_root(err.clone(), "@self/../../outside");
        assert_diagnostic_hides_path(&err, base.path());
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn filesystem_resolver_rejects_symlink_escape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_file = outside.path().join("outside.luau");
        let link = dir.path().join("link.luau");
        fs::write(&outside_file, "return 'outside'").expect("write outside");
        if create_file_symlink(&outside_file, &link).is_err() {
            return;
        }

        let err = FilesystemResolver::new(dir.path())
            .resolve(None, "link")
            .await
            .expect_err("symlink outside root");

        assert_outside_root(err, "link");
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

        assert_filesystem_source(&source, "return 'package'", "init.luau");
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

        assert_filesystem_source(&source, "return 'lua'", "main.lua");
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

        assert_filesystem_source(&source, "return 'package'", "init.luau");
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

        assert_filesystem_source(&source, "return 'dep'", "dep.luau");
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

    #[cfg(unix)]
    fn create_file_symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        symlink_file(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        symlink_file(target, link)
    }
}
