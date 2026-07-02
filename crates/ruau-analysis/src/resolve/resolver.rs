//! Source-resolution traits, virtual implementations, and resolver errors.

#[cfg(any())]
use std::collections::BTreeMap;
use std::{
    borrow::Cow,
    fmt,
    path::{Path, PathBuf},
};

#[cfg(any())]
use ruau_ast::syntax::Expr;
use ruau_source::{ModuleId, ModuleName, ModuleSourceError, is_relative_request};
#[cfg(any())]
use ruau_source::{ModuleSource, ReadRequest, SourceMetadata, poll_ready_once};

/// Source code for one module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceCode {
    /// Source text.
    pub source: String,
}

impl SourceCode {
    /// Wraps the source text of one module.
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

impl AsRef<[u8]> for SourceCode {
    fn as_ref(&self) -> &[u8] {
        self.source.as_bytes()
    }
}

/// Identity of a resolved module reference, threaded back in as the context for
/// resolving relative requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleInfo {
    /// Resolved module name.
    pub name: ModuleName,
}

impl ModuleInfo {
    /// Creates module info for a resolved name.
    #[must_use]
    pub fn new(name: impl Into<ModuleName>) -> Self {
        Self { name: name.into() }
    }
}

/// Result type for source and config resolver adapters.
pub type ResolverResult<T> = Result<T, ResolverError>;

/// Error raised by source and config resolver adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolverError {
    /// The requested module did not resolve to a source file.
    MissingModule {
        /// Module that failed to resolve.
        module: ModuleName,
        /// Directory searched for the module, when known.
        searched: Option<PathBuf>,
    },
    /// More than one source file matched the same module request.
    AmbiguousModule {
        /// Module whose request was ambiguous.
        module: ModuleName,
        /// Directory whose contents collided.
        searched: PathBuf,
    },
    /// A relative request appeared with no module context to resolve against.
    UnresolvableRelativeRequest {
        /// The relative request that could not be resolved.
        request: String,
    },
    /// A resolver alias was malformed.
    InvalidAlias {
        /// The rejected alias spelling.
        alias: String,
        /// Config source that defined the alias.
        source: PathBuf,
    },
    /// A request attempted to require an `init` file directly.
    InitFileRequiredDirectly {
        /// Module whose `init` file was required directly.
        module: ModuleName,
    },
    /// A filesystem-backed request would escape the configured source root.
    ModuleEscapesRoot {
        /// Module whose normalized path would escape the root.
        module: ModuleName,
    },
    /// Both supported config spellings were present in one directory.
    ConfigAmbiguity {
        /// Directory holding the conflicting config files.
        directory: PathBuf,
    },
    /// A config file could not be parsed into portable [`AnalysisConfig`].
    ///
    /// [`AnalysisConfig`]: crate::resolve::config::AnalysisConfig
    ConfigError {
        /// Config file that failed to parse.
        path: PathBuf,
        /// Parser-supplied detail.
        detail: String,
    },
    /// The async-first module source model could not satisfy a ready static
    /// analysis request.
    ModuleSource {
        /// Module being read or resolved, when known.
        module: Option<ModuleName>,
        /// Source-supplied detail.
        detail: String,
    },
    /// The filesystem returned an I/O error reading a source or config file.
    Io {
        /// Path being read when the error occurred.
        path: PathBuf,
        /// Rendered I/O error.
        detail: String,
    },
}

impl ResolverError {
    /// Returns the stable slug identifying this error category.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::MissingModule { .. } => "missing-module",
            Self::AmbiguousModule { .. } => "ambiguous-module",
            Self::UnresolvableRelativeRequest { .. } => "unresolvable-relative-request",
            Self::InvalidAlias { .. } => "invalid-alias",
            Self::InitFileRequiredDirectly { .. } => "init-file-required-directly",
            Self::ModuleEscapesRoot { .. } => "module-escapes-root",
            Self::ConfigAmbiguity { .. } => "config-ambiguity",
            Self::ConfigError { .. } => "config-parse-error",
            Self::ModuleSource { .. } => "module-source",
            Self::Io { .. } => "io-error",
        }
    }

    /// Returns the related module, when the error names one.
    #[must_use]
    pub const fn module(&self) -> Option<&ModuleName> {
        match self {
            Self::MissingModule { module, .. }
            | Self::AmbiguousModule { module, .. }
            | Self::InitFileRequiredDirectly { module }
            | Self::ModuleEscapesRoot { module } => Some(module),
            Self::ModuleSource { module, .. } => module.as_ref(),
            _ => None,
        }
    }

    /// Returns the related filesystem path, when the error names one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::MissingModule { searched, .. } => searched.as_deref(),
            Self::AmbiguousModule { searched, .. } => Some(searched),
            Self::InvalidAlias { source, .. } => Some(source),
            Self::ConfigAmbiguity { directory } => Some(directory),
            Self::ConfigError { path, .. } | Self::Io { path, .. } => Some(path),
            Self::UnresolvableRelativeRequest { .. }
            | Self::InitFileRequiredDirectly { .. }
            | Self::ModuleEscapesRoot { .. }
            | Self::ModuleSource { .. } => None,
        }
    }

    /// Returns human-readable detail, when the error carries any.
    #[must_use]
    pub fn detail(&self) -> Option<Cow<'_, str>> {
        match self {
            Self::InvalidAlias { alias, .. } => Some(Cow::Borrowed(alias.as_str())),
            Self::ConfigError { detail, .. }
            | Self::Io { detail, .. }
            | Self::ModuleSource { detail, .. } => Some(Cow::Borrowed(detail.as_str())),
            Self::UnresolvableRelativeRequest { request } => Some(Cow::Owned(format!(
                "relative request `{request}` has no context"
            ))),
            _ => None,
        }
    }
}

impl fmt::Display for ResolverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingModule { module, searched } => {
                write!(formatter, "module `{module}` did not resolve")?;
                if let Some(searched) = searched {
                    write!(formatter, " (searched {})", searched.display())?;
                }
                Ok(())
            }
            Self::AmbiguousModule { module, searched } => write!(
                formatter,
                "module `{module}` matched more than one source under {}",
                searched.display()
            ),
            Self::UnresolvableRelativeRequest { request } => {
                write!(formatter, "relative request `{request}` has no context")
            }
            Self::InvalidAlias { alias, source } => {
                write!(formatter, "invalid alias `{alias}` in {}", source.display())
            }
            Self::InitFileRequiredDirectly { module } => {
                write!(
                    formatter,
                    "module `{module}` requires an init file directly"
                )
            }
            Self::ModuleEscapesRoot { module } => {
                write!(formatter, "module `{module}` escapes filesystem root")
            }
            Self::ConfigAmbiguity { directory } => write!(
                formatter,
                "conflicting config files in {}",
                directory.display()
            ),
            Self::ConfigError { path, detail } => {
                write!(
                    formatter,
                    "failed to parse config {}: {detail}",
                    path.display()
                )
            }
            Self::ModuleSource { module, detail } => {
                if let Some(module) = module {
                    write!(formatter, "module source error for `{module}`: {detail}")
                } else {
                    write!(formatter, "module source error: {detail}")
                }
            }
            Self::Io { path, detail } => {
                write!(formatter, "I/O error reading {}: {detail}", path.display())
            }
        }
    }
}

impl std::error::Error for ResolverError {}

/// Source resolver shaped after upstream `FileResolver`.
///
/// Hidden development scaffolding for upstream fixture tests and transitional
/// graph internals. Public callers should provide [`ModuleSource`] to
/// [`crate::Frontend::new`] instead.
#[doc(hidden)]
#[cfg(any())]
pub trait FileResolver: Sync {
    /// Reads source for a module, reporting why resolution failed.
    fn read_source(&self, name: &ModuleName) -> ResolverResult<SourceCode>;

    /// Resolves a module expression relative to an optional context.
    ///
    /// Returns `Ok(None)` when `expr` is not a module reference, and `Err(_)`
    /// when it is a module reference that failed to resolve.
    fn resolve_module(
        &self,
        context: Option<&ModuleInfo>,
        expr: &Expr,
    ) -> ResolverResult<Option<ModuleInfo>>;

    /// Returns display metadata for a module.
    fn module_metadata(&self, name: &ModuleName) -> SourceMetadata {
        SourceMetadata {
            display_name: name.to_string(),
            environment: None,
        }
    }
}

/// Ready-only [`FileResolver`] facade over the async-first [`ModuleSource`].
///
/// This lets today's static frontend consume the same source model as runtime
/// `require` while source-graph analysis is being made fully async. Source
/// futures that return `Poll::Pending` become resolver diagnostics.
#[doc(hidden)]
#[cfg(any())]
pub struct ReadyModuleSourceFiles<'source> {
    source: &'source dyn ModuleSource,
}

#[cfg(any())]
impl<'source> ReadyModuleSourceFiles<'source> {
    /// Creates a ready-only facade over `source`.
    #[must_use]
    pub const fn new(source: &'source dyn ModuleSource) -> Self {
        Self { source }
    }
}

#[cfg(any())]
impl FileResolver for ReadyModuleSourceFiles<'_> {
    fn read_source(&self, name: &ModuleName) -> ResolverResult<SourceCode> {
        let id = ModuleId::from(name);
        let bytes = poll_ready_once(
            self.source.read_request(ReadRequest::new(&id)),
            "reading module source",
        )
        .map_err(|error| resolver_error_from_module_source(error, Some(name.clone())))?;
        String::from_utf8(bytes)
            .map(SourceCode::new)
            .map_err(|error| ResolverError::ModuleSource {
                module: Some(name.clone()),
                detail: format!("source is not UTF-8: {error}"),
            })
    }

    fn resolve_module(
        &self,
        context: Option<&ModuleInfo>,
        expr: &Expr,
    ) -> ResolverResult<Option<ModuleInfo>> {
        let Expr::String { value, .. } = expr else {
            return Ok(None);
        };

        let requester = context.map(|info| ModuleId::from(&info.name));
        let context_name = context.map(|info| info.name.clone());
        let id = poll_ready_once(
            self.source.resolve(requester.as_ref(), value.as_bytes()),
            "resolving module source",
        )
        .map_err(|error| resolver_error_from_module_source(error, context_name))?;
        let name = ModuleName::from_id(&id).map_err(|error| ResolverError::ModuleSource {
            module: None,
            detail: error.to_string(),
        })?;
        Ok(Some(ModuleInfo::new(name)))
    }

    fn module_metadata(&self, name: &ModuleName) -> SourceMetadata {
        self.source.metadata(&ModuleId::from(name))
    }
}

/// In-memory source resolver for virtual sources and tests.
///
/// Hidden development scaffolding for tests that need resolver-shaped mutation
/// or metadata. Public callers should use [`ruau_source::InMemorySource`].
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
#[cfg(any())]
pub struct InMemorySourceResolver {
    /// Known source files.
    sources: BTreeMap<ModuleName, SourceCode>,
    /// Optional display names.
    display_names: BTreeMap<ModuleName, String>,
    /// Optional environment names.
    environments: BTreeMap<ModuleName, String>,
}

#[cfg(any())]
impl InMemorySourceResolver {
    /// Creates an empty resolver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts source for a module.
    pub fn insert(&mut self, name: impl Into<ModuleName>, source: SourceCode) {
        self.sources.insert(name.into(), source);
    }

    /// Sets the human-readable display name for a module.
    pub fn set_display_name(&mut self, name: impl Into<ModuleName>, display_name: String) {
        self.display_names.insert(name.into(), display_name);
    }

    /// Sets the environment for a module.
    pub fn set_environment(&mut self, name: impl Into<ModuleName>, environment: String) {
        self.environments.insert(name.into(), environment);
    }
}

#[cfg(any())]
impl FileResolver for InMemorySourceResolver {
    fn read_source(&self, name: &ModuleName) -> ResolverResult<SourceCode> {
        self.sources
            .get(name)
            .cloned()
            .ok_or_else(|| ResolverError::MissingModule {
                module: name.clone(),
                searched: None,
            })
    }

    fn resolve_module(
        &self,
        context: Option<&ModuleInfo>,
        expr: &Expr,
    ) -> ResolverResult<Option<ModuleInfo>> {
        let Expr::String { value, .. } = expr else {
            return Ok(None);
        };

        let requested = resolve_requested_module_name(context, value)?;
        Ok(Some(ModuleInfo::new(requested)))
    }

    fn module_metadata(&self, name: &ModuleName) -> SourceMetadata {
        SourceMetadata {
            display_name: self
                .display_names
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_string()),
            environment: self.environments.get(name).cloned(),
        }
    }
}

/// Resolves a module request string into a normalized portable module name.
///
/// # Errors
///
/// Returns [`ResolverError::UnresolvableRelativeRequest`] when `request` is
/// relative but `context` is absent.
pub fn resolve_requested_module_name(
    context: Option<&ModuleInfo>,
    request: &str,
) -> ResolverResult<ModuleName> {
    if !is_relative_request(request) {
        return Ok(ModuleName::normalize(request));
    }

    let Some(context) = context else {
        return Err(ResolverError::UnresolvableRelativeRequest {
            request: request.to_owned(),
        });
    };
    let base = context.name.parent();
    Ok(ModuleName::join(base.as_str(), request))
}

pub fn resolver_error_from_module_source(
    error: ModuleSourceError,
    module: Option<ModuleName>,
) -> ResolverError {
    match error {
        ModuleSourceError::MissingModule { id } => ResolverError::MissingModule {
            module: module_name_from_lossy_id(&id),
            searched: None,
        },
        ModuleSourceError::UnresolvableRelativeRequest { request } => {
            ResolverError::UnresolvableRelativeRequest {
                request: String::from_utf8_lossy(&request).into_owned(),
            }
        }
        ModuleSourceError::Pending { .. } | ModuleSourceError::Other { .. } => {
            ResolverError::ModuleSource {
                module,
                detail: error.to_string(),
            }
        }
    }
}

fn module_name_from_lossy_id(id: &ModuleId) -> ModuleName {
    id.as_str()
        .map(ModuleName::from)
        .unwrap_or_else(|| ModuleName::from(id.to_lossy_string()))
}
