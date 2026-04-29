//! Shared module-resolution contracts for runtime loading and analysis.
#![allow(clippy::missing_docs_in_private_items)]

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
    result::Result as StdResult,
    sync::Arc,
};

pub use crate::analyzer::CancellationToken;

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

/// Source text and optional filesystem path for one resolved module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSource {
    /// Stable module id.
    pub id: ModuleId,
    /// Source text.
    pub source: String,
    /// Filesystem path when this module came from disk.
    pub path: Option<PathBuf>,
}

/// Resolved module returned by a [`ModuleResolver`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModule {
    /// Stable module id.
    pub id: ModuleId,
    /// Source text.
    pub source: String,
    /// Filesystem path when this module came from disk.
    pub path: Option<PathBuf>,
}

impl ResolvedModule {
    /// Converts the resolved module into snapshot source storage.
    #[must_use]
    pub fn into_source(self) -> ModuleSource {
        ModuleSource {
            id: self.id,
            source: self.source,
            path: self.path,
        }
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
    ) -> StdResult<ResolvedModule, ModuleResolveError>;
}

impl<T> ModuleResolver for Arc<T>
where
    T: ModuleResolver,
{
    fn resolve(
        &self,
        requester: Option<&ModuleId>,
        specifier: &str,
    ) -> StdResult<ResolvedModule, ModuleResolveError> {
        (**self).resolve(requester, specifier)
    }
}

/// Module resolution failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleResolveError {
    /// The requested module was not found.
    NotFound(String),
    /// The requested module path is ambiguous.
    Ambiguous(String),
    /// The module could not be read.
    Read {
        /// Module label or path.
        module: String,
        /// Human-readable read error.
        message: String,
    },
}

impl std::fmt::Display for ModuleResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(module) => write!(formatter, "module not found: {module}"),
            Self::Ambiguous(module) => write!(formatter, "module is ambiguous: {module}"),
            Self::Read { module, message } => {
                write!(formatter, "failed to read {module}: {message}")
            }
        }
    }
}

impl std::error::Error for ModuleResolveError {}

/// Immutable resolved graph used by checked loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverSnapshot {
    /// Root module id.
    pub root: ModuleId,
    /// Resolved modules keyed by id.
    pub modules: BTreeMap<ModuleId, ModuleSource>,
    /// Resolved dependency edges keyed by requesting module and original specifier.
    pub edges: BTreeMap<ModuleId, BTreeMap<String, ModuleId>>,
}

impl ResolverSnapshot {
    /// Resolves a root module and its direct string-literal dependencies.
    pub fn resolve<R: ModuleResolver>(
        resolver: &R,
        root: impl Into<ModuleId>,
    ) -> StdResult<Self, ModuleResolveError> {
        let root = resolver.resolve(None, root.into().as_str())?.into_source();
        let root_id = root.id.clone();
        let mut modules = BTreeMap::new();
        modules.insert(root.id.clone(), root);
        let mut edges = BTreeMap::new();
        let mut queue = VecDeque::from([root_id.clone()]);
        let mut queued = HashSet::from([root_id.clone()]);

        while let Some(id) = queue.pop_front() {
            let source = modules
                .get(&id)
                .expect("queued resolver snapshot module is missing")
                .clone();
            let requires = string_requires(&source.source);
            for required in requires {
                let dep = resolver.resolve(Some(&source.id), &required)?;
                edges
                    .entry(source.id.clone())
                    .or_insert_with(BTreeMap::new)
                    .insert(required, dep.id.clone());
                if queued.insert(dep.id.clone()) {
                    let dep_id = dep.id.clone();
                    modules.insert(dep.id.clone(), dep.into_source());
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

    /// Returns the root module source.
    #[must_use]
    pub fn root_source(&self) -> Option<&ModuleSource> {
        self.modules.get(&self.root)
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
                source: module.source.as_str(),
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
        self.modules.insert(id.into(), source.into());
        self
    }
}

impl ModuleResolver for InMemoryResolver {
    fn resolve(
        &self,
        requester: Option<&ModuleId>,
        specifier: &str,
    ) -> StdResult<ResolvedModule, ModuleResolveError> {
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
        Ok(ResolvedModule {
            id,
            source: source.clone(),
            path: None,
        })
    }
}

/// Filesystem resolver that matches the runtime require extension order.
#[derive(Debug, Clone)]
pub struct FilesystemResolver {
    root: PathBuf,
}

impl FilesystemResolver {
    /// Creates a filesystem resolver rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl ModuleResolver for FilesystemResolver {
    fn resolve(
        &self,
        requester: Option<&ModuleId>,
        specifier: &str,
    ) -> StdResult<ResolvedModule, ModuleResolveError> {
        let base = if let Some(requester) = requester {
            Path::new(requester.as_str())
                .parent()
                .map_or_else(|| self.root.clone(), Path::to_path_buf)
        } else {
            self.root.clone()
        };
        let candidate = Path::new(specifier);
        let logical = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            base.join(candidate)
        };
        let path = resolve_module_file(&logical)?;
        let source = fs::read_to_string(&path).map_err(|error| ModuleResolveError::Read {
            module: path.display().to_string(),
            message: error.to_string(),
        })?;
        Ok(ResolvedModule {
            id: ModuleId::new(path.display().to_string()),
            source,
            path: Some(path),
        })
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

fn string_requires(source: &str) -> Vec<String> {
    let mut requires = Vec::new();
    let mut rest = source;
    while let Some(index) = rest.find("require(") {
        rest = &rest[index + "require(".len()..];
        let trimmed = rest.trim_start();
        let Some(quote) = trimmed
            .chars()
            .next()
            .filter(|ch| *ch == '"' || *ch == '\'')
        else {
            continue;
        };
        let after_quote = &trimmed[quote.len_utf8()..];
        if let Some(end) = after_quote.find(quote) {
            requires.push(after_quote[..end].to_owned());
            rest = &after_quote[end + quote.len_utf8()..];
        }
    }
    requires
}
