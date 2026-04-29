//! Shared module-resolution contracts for runtime loading and analysis.
#![allow(clippy::missing_docs_in_private_items)]

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt, fs,
    path::{Component, Path, PathBuf},
    result::Result as StdResult,
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
    T: ModuleResolver,
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

/// Filesystem resolver that matches the runtime require extension order.
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

fn string_requires(source: &str) -> Vec<String> {
    let mut requires = Vec::new();
    let bytes = source.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if let Some((content_start, equals_count)) = long_bracket_content_start(bytes, index) {
            index = skip_long_bracket(bytes, content_start, equals_count);
            continue;
        }

        match bytes[index] {
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = skip_comment(bytes, index + 2);
            }
            b'\'' | b'"' => {
                index = skip_quoted_string(bytes, index);
            }
            _ if starts_keyword(bytes, index, b"require") => {
                if let Some((specifier, next_index)) = parse_require(source, index) {
                    requires.push(specifier);
                    index = next_index;
                } else {
                    index += b"require".len();
                }
            }
            _ => index += 1,
        }
    }

    requires
}

fn parse_require(source: &str, start: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut index = skip_whitespace(bytes, start + b"require".len());
    if bytes.get(index) != Some(&b'(') {
        return None;
    }

    index = skip_whitespace(bytes, index + 1);
    let quote = *bytes
        .get(index)
        .filter(|quote| **quote == b'\'' || **quote == b'"')?;
    let string_start = index + 1;
    let mut escaped = false;
    index = string_start;

    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            return Some((source[string_start..index].to_owned(), index + 1));
        }
        index += 1;
    }

    None
}

fn starts_keyword(bytes: &[u8], index: usize, keyword: &[u8]) -> bool {
    let end = index + keyword.len();
    bytes.get(index..end) == Some(keyword)
        && index
            .checked_sub(1)
            .and_then(|before| bytes.get(before))
            .is_none_or(|byte| !is_ident_byte(*byte))
        && bytes.get(end).is_none_or(|byte| !is_ident_byte(*byte))
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn skip_comment(bytes: &[u8], mut index: usize) -> usize {
    if let Some((content_start, equals_count)) = long_bracket_content_start(bytes, index) {
        return skip_long_bracket(bytes, content_start, equals_count);
    }

    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

fn long_bracket_content_start(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    if bytes.get(index) != Some(&b'[') {
        return None;
    }

    let mut cursor = index + 1;
    while bytes.get(cursor) == Some(&b'=') {
        cursor += 1;
    }

    (bytes.get(cursor) == Some(&b'[')).then_some((cursor + 1, cursor - index - 1))
}

fn skip_long_bracket(bytes: &[u8], mut index: usize, equals_count: usize) -> usize {
    while index < bytes.len() {
        if bytes[index] == b']' {
            let cursor = index + 1;
            let equals_end = cursor + equals_count;
            if bytes.get(cursor..equals_end).is_some_and(|equals| {
                equals.iter().all(|byte| *byte == b'=')
            }) && bytes.get(equals_end) == Some(&b']')
            {
                return equals_end + 1;
            }
            index = equals_end;
        } else {
            index += 1;
        }
    }
    bytes.len()
}

fn skip_quoted_string(bytes: &[u8], mut index: usize) -> usize {
    let quote = bytes[index];
    let mut escaped = false;
    index += 1;

    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            return index + 1;
        }
        index += 1;
    }

    bytes.len()
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::{string_requires, InMemoryResolver, ModuleId, ResolverSnapshot};

    #[test]
    fn string_requires_ignores_comments_and_strings() {
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

        assert_eq!(vec!["dep"], string_requires(source));
    }

    #[test]
    fn string_requires_accepts_whitespace_before_literal() {
        assert_eq!(vec!["dep"], string_requires(r#"return require ( "dep" )"#));
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
