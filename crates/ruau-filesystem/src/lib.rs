//! Filesystem-backed module sources and config materializers.

use std::{
    fmt, fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    str,
    string::FromUtf8Error,
    sync::Arc,
};

use cap_std::{ambient_authority, fs::Dir};
use ruau_source::{
    InstanceKey, ModuleId, ModuleName, ReadContext, Source, SourceEpoch, SourceError, SourceFuture,
    SourceMetadata, SourceProvider, SourceRead, ready,
};
use ruau_syntax::{Expr, Stat, TableItem, parse::parse};
use ruau_typecheck::{
    Mode,
    config::{
        Alias, ModuleConfig, ModuleInfo, Origin, Resolver, ResolverError, ResolverResult,
        is_valid_alias, resolve_requested_module_name,
    },
};
use same_file::Handle;

mod error;
mod mounts;

pub use error::DirectoryError;
pub use mounts::{DirectoryMounts, DirectoryMountsBuilder, DirectoryMountsError};

/// Default byte cap for one filesystem-backed source or config file.
pub const DEFAULT_MAX_READ_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedSource {
    module: ModuleName,
    display_path: PathBuf,
}

#[derive(Clone, Debug)]
struct FilesystemSourceResolver {
    root: ValidatedRoot,
    max_read_bytes: usize,
}

impl FilesystemSourceResolver {
    fn new(root: impl AsRef<Path>) -> Result<Self, DirectoryError> {
        Ok(Self {
            root: ValidatedRoot::new(root)?,
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
        })
    }

    #[cfg(any())]
    fn with_max_read_bytes(mut self, max_read_bytes: usize) -> Self {
        self.max_read_bytes = max_read_bytes;
        self
    }

    fn resolve_module_name(&self, name: &ModuleName) -> ResolverResult<ResolvedSource> {
        self.open_module(name).map(|opened| opened.resolved)
    }

    fn open_module(&self, name: &ModuleName) -> ResolverResult<OpenedSource> {
        let module_name = ModuleName::normalize(name.as_str());
        if module_name_escapes_root(module_name.as_str()) {
            return Err(ResolverError::ModuleEscapesRoot {
                module: module_name,
            });
        }
        if is_direct_init_module(module_name.as_str()) {
            return Err(ResolverError::InitFileRequiredDirectly {
                module: name.clone(),
            });
        }

        let base = module_name_to_path(module_name.as_str());
        let mut candidates = Vec::new();
        for path in source_file_candidates(&base) {
            if let Some(opened) = self.open_candidate(module_name.as_str(), path)? {
                candidates.push(opened);
            }
        }
        for path in init_file_candidates(&base) {
            if let Some(opened) = self.open_candidate(module_name.as_str(), path)? {
                candidates.push(opened);
            }
        }

        match candidates.len() {
            0 => Err(ResolverError::MissingModule {
                module: name.clone(),
                searched: Some(self.root.path().join(base)),
            }),
            1 => Ok(candidates.remove(0)),
            _ => Err(ResolverError::AmbiguousModule {
                module: name.clone(),
                searched: self.root.path().join(base),
            }),
        }
    }

    fn open_candidate(
        &self,
        module_name: &str,
        display_path: PathBuf,
    ) -> ResolverResult<Option<OpenedSource>> {
        match self.root.open_file(&display_path) {
            Ok(Some(file)) => Ok(Some(OpenedSource {
                resolved: resolved_module_source(module_name, display_path),
                file,
            })),
            Ok(None) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_diagnostic(&display_path, &error)),
        }
    }

    fn resolve_request(
        &self,
        context: Option<&ModuleInfo>,
        request: &str,
    ) -> ResolverResult<ModuleInfo> {
        let requested = resolve_requested_module_name(context, request)?;
        let resolved = self.resolve_module_name(&requested)?;
        Ok(ModuleInfo::new(resolved.module))
    }

    #[cfg(any())]
    fn read_module_source(&self, name: &ModuleName) -> ResolverResult<String> {
        let opened = self.open_module(name)?;
        let path = opened.file.origin.clone();
        let source = String::from_utf8(
            opened
                .file
                .read_bounded(self.max_read_bytes)
                .map_err(|error| bounded_read_resolver_error(&path, error))?,
        )
        .map_err(|error| {
            bounded_read_resolver_error(&path, BoundedReadError::InvalidUtf8(error))
        })?;
        Ok(source)
    }
}

#[derive(Clone, Debug)]
struct ValidatedRoot {
    requested: PathBuf,
    path: PathBuf,
    directory: Arc<Dir>,
}

impl ValidatedRoot {
    fn new(root: impl AsRef<Path>) -> Result<Self, DirectoryError> {
        let requested = root.as_ref().to_path_buf();
        let path =
            fs::canonicalize(&requested).map_err(|error| DirectoryError::CanonicalizeRoot {
                root: requested.clone(),
                message: error.to_string(),
            })?;
        let directory = Dir::open_ambient_dir(&path, ambient_authority()).map_err(|error| {
            if error.kind() == io::ErrorKind::NotADirectory {
                return DirectoryError::RootNotDirectory { root: path.clone() };
            }
            DirectoryError::OpenRoot {
                root: path.clone(),
                message: error.to_string(),
            }
        })?;
        let metadata = directory
            .dir_metadata()
            .map_err(|error| DirectoryError::OpenRoot {
                root: path.clone(),
                message: error.to_string(),
            })?;
        if !metadata.is_dir() {
            return Err(DirectoryError::RootNotDirectory { root: path });
        }
        Ok(Self {
            requested,
            path,
            directory: Arc::new(directory),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn open_file(&self, relative: &Path) -> io::Result<Option<OpenedFile>> {
        let requested = self.path.join(relative);
        let resolved = match fs::canonicalize(&requested) {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let contained = resolved.strip_prefix(&self.path).map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "filesystem path resolves outside its validated root",
            )
        })?;
        let file = match self.directory.open(contained) {
            Ok(file) => file.into_std(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Ok(None);
        }

        let canonical_relative = self.directory.canonicalize(contained)?;
        let origin = self.path.join(canonical_relative);
        let opened_handle = Handle::from_file(file.try_clone()?)?;
        let origin_handle = Handle::from_path(&origin)?;
        if opened_handle != origin_handle {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "filesystem file changed while it was opened",
            ));
        }

        Ok(Some(OpenedFile {
            file,
            length: metadata.len(),
            origin,
        }))
    }
}

#[derive(Debug)]
pub(crate) struct OpenedFile {
    file: fs::File,
    length: u64,
    origin: PathBuf,
}

impl OpenedFile {
    pub(crate) fn origin(&self) -> &Path {
        &self.origin
    }

    pub(crate) fn read_bounded(self, max_bytes: usize) -> Result<Vec<u8>, BoundedReadError> {
        read_bounded_file(self.file, self.length, max_bytes)
    }
}

#[derive(Debug)]
struct OpenedSource {
    resolved: ResolvedSource,
    file: OpenedFile,
}

/// Filesystem-backed [`SourceProvider`] adapter.
///
/// Reads are blocking and immediately ready. Portable module names cannot
/// escape `root`.
#[derive(Clone, Debug)]
pub struct Directory {
    resolver: FilesystemSourceResolver,
    epoch: SourceEpoch,
}

impl Directory {
    /// Creates a filesystem module source rooted at `root`.
    ///
    /// # Errors
    /// Returns [`DirectoryError`] when `root` is missing, is not a directory,
    /// or cannot be opened as a validated filesystem capability.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, DirectoryError> {
        Self::with_epoch(root, 0)
    }

    /// Creates a filesystem module source with cache epoch `epoch`.
    ///
    /// # Errors
    /// Returns [`DirectoryError`] when `root` cannot be validated.
    pub fn with_epoch(root: impl AsRef<Path>, epoch: u64) -> Result<Self, DirectoryError> {
        Self::with_epoch_handle(root, SourceEpoch::new(epoch))
    }

    /// Creates a filesystem module source with a shared epoch handle.
    ///
    /// # Errors
    /// Returns [`DirectoryError`] when `root` cannot be validated.
    pub fn with_epoch_handle(
        root: impl AsRef<Path>,
        epoch: SourceEpoch,
    ) -> Result<Self, DirectoryError> {
        Ok(Self {
            resolver: FilesystemSourceResolver::new(root)?,
            epoch,
        })
    }

    /// Sets the maximum bytes read for one source file.
    ///
    /// Files over this cap are rejected before allocating their full contents.
    #[must_use]
    pub fn with_max_read_bytes(mut self, max_read_bytes: usize) -> Self {
        self.resolver.max_read_bytes = max_read_bytes;
        self
    }

    /// Returns a clone of the source epoch handle.
    #[must_use]
    pub fn epoch_handle(&self) -> SourceEpoch {
        self.epoch.clone()
    }

    /// Returns a config resolver backed by this directory's validated root.
    ///
    /// This clones the open root capability instead of reopening the
    /// caller-supplied path.
    #[must_use]
    pub fn config_resolver(&self) -> FilesystemResolver {
        FilesystemResolver::from_validated_root(self.resolver.root.clone())
    }

    pub(crate) fn root(&self) -> &Path {
        self.resolver.root.path()
    }

    pub(crate) fn open_path(&self, relative: &Path) -> io::Result<Option<OpenedFile>> {
        self.resolver.root.open_file(relative)
    }

    fn module_name_from_id(id: &ModuleId) -> Result<ModuleName, SourceError> {
        ModuleName::from_id(id)
    }

    fn display_name(&self, resolved: &ResolvedSource) -> String {
        resolved.display_path.to_string_lossy().into_owned()
    }

    fn read_source(&self, id: &ModuleId, epoch: u64) -> Result<SourceRead, SourceError> {
        let name = Self::module_name_from_id(id)?;
        let opened = self
            .resolver
            .open_module(&name)
            .map_err(module_source_error_from_resolver)?;
        let display_name = self.display_name(&opened.resolved);
        let origin = opened.file.origin.clone();
        let bytes = opened
            .file
            .read_bounded(self.resolver.max_read_bytes)
            .map_err(|error| bounded_read_module_source_error(&display_name, error))?;
        str::from_utf8(&bytes).map_err(|error| {
            SourceError::other(format!("source {display_name} is not UTF-8: {error}"))
        })?;
        let observed = self.epoch.get();
        if observed != epoch {
            return Err(SourceError::EpochChanged {
                expected: epoch,
                observed,
            });
        }
        let source =
            Source::bytes(id.clone(), bytes).with_metadata(SourceMetadata::new(display_name));
        Ok(SourceRead::new(
            source,
            InstanceKey::shared(id.clone()),
            epoch,
            Some(origin),
        ))
    }
}

impl SourceProvider for Directory {
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId> {
        let result = (|| {
            let request = str::from_utf8(request).map_err(|error| {
                SourceError::other(format!("module request is not UTF-8: {error}"))
            })?;
            let requester = requester
                .map(Self::module_name_from_id)
                .transpose()?
                .map(ModuleInfo::new);
            let module = self
                .resolver
                .resolve_request(requester.as_ref(), request)
                .map_err(module_source_error_from_resolver)?;
            Ok(ModuleId::from(&module.name))
        })();
        ready(result)
    }

    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
        let epoch = self.epoch.get();
        let result = self
            .read_source(id, epoch)
            .map(|observation| observation.into_parts().0.into_bytes());
        ready(result)
    }

    fn read_observation(&self, request: ReadContext<'_>) -> SourceFuture<SourceRead> {
        ready(self.read_source(request.id(), self.epoch.get()))
    }

    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        let Ok(name) = Self::module_name_from_id(id) else {
            return SourceMetadata::new(id.to_diagnostic_string());
        };
        match self.resolver.resolve_module_name(&name) {
            Ok(resolved) => SourceMetadata::new(self.display_name(&resolved)),
            Err(_) => SourceMetadata::new(name.to_string()),
        }
    }

    fn epoch(&self) -> u64 {
        self.epoch.get()
    }
}

fn module_source_error_from_resolver(error: ResolverError) -> SourceError {
    match error {
        ResolverError::MissingModule { module, .. } => SourceError::MissingModule {
            id: ModuleId::canonicalized(module.as_str()),
        },
        ResolverError::UnresolvableRelativeRequest { request } => {
            SourceError::UnresolvableRelativeRequest {
                request: request.into_bytes(),
            }
        }
        ResolverError::ModuleEscapesRoot { module } => {
            SourceError::other(format!("module `{module}` escapes filesystem root"))
        }
        other => SourceError::other(other.to_string()),
    }
}

/// Filesystem-backed config materializer.
///
/// Portable module names cannot escape `root`.
#[derive(Clone, Debug)]
pub struct FilesystemResolver {
    /// Caller-facing root used in diagnostics.
    display_root: PathBuf,
    /// Validated root used for config I/O.
    root: ValidatedRoot,
    max_read_bytes: usize,
}

impl FilesystemResolver {
    /// Creates a filesystem config resolver rooted at `root`.
    ///
    /// Config files are opened through a validated root capability. Config
    /// symlinks that resolve outside this root are rejected.
    ///
    /// # Errors
    /// Returns [`DirectoryError`] when `root` is missing, is not a directory,
    /// or cannot be opened as a validated filesystem capability.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, DirectoryError> {
        Ok(Self::from_validated_root(ValidatedRoot::new(root)?))
    }

    fn from_validated_root(root: ValidatedRoot) -> Self {
        Self {
            display_root: root.requested.clone(),
            root,
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
        }
    }

    /// Returns the resolver root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.display_root
    }

    /// Sets the maximum bytes read for one config file.
    ///
    /// Files over this cap are rejected before allocating their full contents.
    #[must_use]
    pub fn with_max_read_bytes(mut self, max_read_bytes: usize) -> Self {
        self.max_read_bytes = max_read_bytes;
        self
    }
}

impl Resolver for FilesystemResolver {
    fn config_for_module(&self, name: &ModuleName) -> ResolverResult<ModuleConfig> {
        let module_name = ModuleName::normalize(name.as_str());
        if module_name_escapes_root(module_name.as_str()) {
            return Err(ResolverError::ModuleEscapesRoot {
                module: module_name,
            });
        }
        let module_parent = module_name.parent().as_str().to_owned();
        let mut config = ModuleConfig::new();

        for directory in config_search_directories(&module_parent) {
            if let Some(directory_config) = load_config_from_directory(
                &self.root,
                &self.display_root,
                &directory,
                self.max_read_bytes,
            )? {
                config = config.merged_with(&directory_config);
            }
        }

        Ok(config)
    }
}

/// Returns whether `module_name` names an init module directly.
fn is_direct_init_module(module_name: &str) -> bool {
    module_name
        .rsplit('/')
        .next()
        .is_some_and(|component| component == "init")
}

/// Converts a portable module name to a platform path.
fn module_name_to_path(module_name: &str) -> PathBuf {
    module_name
        .split('/')
        .filter(|component| !component.is_empty())
        .collect()
}

fn module_name_escapes_root(module_name: &str) -> bool {
    Path::new(module_name).components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) || module_name
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .any(|component| {
            component == ".." || component.ends_with(':') || has_windows_drive_prefix(component)
        })
}

fn has_windows_drive_prefix(component: &str) -> bool {
    let bytes = component.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Builds a resolved module-source candidate.
fn resolved_module_source(module_name: &str, display_path: PathBuf) -> ResolvedSource {
    ResolvedSource {
        module: ModuleName::new(module_name),
        display_path,
    }
}

/// Returns candidates for a source file module.
fn source_file_candidates(base: &Path) -> Vec<PathBuf> {
    if has_source_extension(base) {
        vec![base.to_path_buf()]
    } else {
        vec![
            append_extension(base, "luau"),
            append_extension(base, "lua"),
        ]
    }
}

/// Returns candidates for an init-file module.
fn init_file_candidates(base: &Path) -> [PathBuf; 2] {
    [base.join("init.luau"), base.join("init.lua")]
}

/// Returns whether a path already has a Luau source extension.
fn has_source_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "luau" | "lua"))
}

/// Appends a source extension instead of replacing a dotted path component.
fn append_extension(path: &Path, extension: &str) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.to_string_lossy(), extension))
}

/// Returns directories searched while materializing config for a module.
fn config_search_directories(module_parent: &str) -> Vec<PathBuf> {
    let mut directories = vec![PathBuf::new()];
    let mut current = PathBuf::new();
    for component in module_parent
        .split('/')
        .filter(|component| !component.is_empty())
    {
        current.push(component);
        directories.push(current.clone());
    }
    directories
}

/// Loads one config file from a directory.
fn load_config_from_directory(
    root: &ValidatedRoot,
    display_root: &Path,
    directory: &Path,
    max_read_bytes: usize,
) -> ResolverResult<Option<ModuleConfig>> {
    let luaurc_relative = directory.join(".luaurc");
    let config_luau_relative = directory.join(".config.luau");
    let luaurc = display_root.join(&luaurc_relative);
    let config_luau = display_root.join(&config_luau_relative);
    let luaurc_file = root
        .open_file(&luaurc_relative)
        .map_err(|error| io_diagnostic(&luaurc, &error))?;
    let config_luau_file = root
        .open_file(&config_luau_relative)
        .map_err(|error| io_diagnostic(&config_luau, &error))?;

    match (luaurc_file, config_luau_file) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(ResolverError::ConfigAmbiguity {
            directory: display_root.join(directory),
        }),
        (Some(file), None) => parse_luaurc_file(&luaurc, file, max_read_bytes).map(Some),
        (None, Some(file)) => parse_config_luau_file(&config_luau, file, max_read_bytes).map(Some),
    }
}

/// Parses a `.luaurc` file into portable config.
fn parse_luaurc_file(
    path: &Path,
    file: OpenedFile,
    max_read_bytes: usize,
) -> ResolverResult<ModuleConfig> {
    let contents = read_config_file(path, file, max_read_bytes)?;
    let contents = normalize_luaurc_json(&contents);
    let value: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|error| config_parse_error(path, error.to_string()))?;
    let mut config = ModuleConfig::new();

    decode_luaurc_config(&value, path)?.apply_to(&mut config, path)?;

    Ok(config)
}

/// Parses a `.config.luau` file into portable config.
fn parse_config_luau_file(
    path: &Path,
    file: OpenedFile,
    max_read_bytes: usize,
) -> ResolverResult<ModuleConfig> {
    let contents = read_config_file(path, file, max_read_bytes)?;
    let result = parse(&contents);
    if let Some(error) = result.errors.first() {
        let line = error.location.begin.line + 1;
        let column = error.location.begin.column + 1;
        return Err(config_parse_error(
            path,
            format!("{} (at line {}, column {})", error.message, line, column),
        ));
    }

    let mut config = ModuleConfig::new();
    let Stat::Block { body, .. } = &result.root else {
        return Ok(config);
    };
    decode_config_luau_fields(body, path)?.apply_to(&mut config, path)?;

    Ok(config)
}

/// Normalizes Luau's JSON-ish `.luaurc` syntax into strict JSON.
fn normalize_luaurc_json(contents: &str) -> String {
    remove_trailing_json_commas(&strip_json_comments(contents))
}

fn strip_json_comments(contents: &str) -> String {
    let mut stripped = String::with_capacity(contents.len());
    let mut chars = contents.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(character) = chars.next() {
        if in_string {
            stripped.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }

        if character == '"' {
            in_string = true;
            stripped.push(character);
        } else if character == '/' && chars.peek() == Some(&'/') {
            let _ = chars.next();
            for comment_character in chars.by_ref() {
                if comment_character == '\n' {
                    stripped.push('\n');
                    break;
                }
            }
        } else {
            stripped.push(character);
        }
    }

    stripped
}

fn remove_trailing_json_commas(contents: &str) -> String {
    let chars = contents.chars().collect::<Vec<_>>();
    let mut normalized = String::with_capacity(contents.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;

    while index < chars.len() {
        let character = chars[index];
        if in_string {
            normalized.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if character == '"' {
            in_string = true;
            normalized.push(character);
            index += 1;
            continue;
        }

        if character == ',' {
            let mut lookahead = index + 1;
            while lookahead < chars.len() && chars[lookahead].is_whitespace() {
                lookahead += 1;
            }
            if matches!(chars.get(lookahead).copied(), Some('}' | ']')) {
                index += 1;
                continue;
            }
        }

        normalized.push(character);
        index += 1;
    }

    normalized
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ConfigPatch {
    mode: Option<Mode>,
    lint_errors: Option<bool>,
    type_errors: Option<bool>,
    globals: Option<Vec<String>>,
    aliases: Vec<(String, String)>,
}

impl ConfigPatch {
    fn apply_to(self, config: &mut ModuleConfig, path: &Path) -> ResolverResult<()> {
        if let Some(mode) = self.mode {
            config.set_mode(mode);
        }
        if let Some(lint_errors) = self.lint_errors {
            config.set_lint_errors(lint_errors);
        }
        if let Some(type_errors) = self.type_errors {
            config.set_type_errors(type_errors);
        }
        if let Some(globals) = self.globals {
            config.set_globals(globals);
        }
        for (alias, target) in self.aliases {
            set_materialized_alias(config, &alias, &target, path)?;
        }
        Ok(())
    }
}

#[allow(clippy::field_reassign_with_default)]
fn decode_luaurc_config(value: &serde_json::Value, path: &Path) -> ResolverResult<ConfigPatch> {
    let mut patch = ConfigPatch::default();

    patch.mode = value
        .get("languageMode")
        .and_then(serde_json::Value::as_str)
        .map(|mode| parse_config_mode(mode, false, path))
        .transpose()?;

    if let Some(language) = value.get("language").and_then(serde_json::Value::as_object)
        && let Some(mode) = language
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .map(|mode| parse_config_mode(mode, true, path))
            .transpose()?
    {
        patch.mode = Some(mode);
    }

    patch.lint_errors = value
        .get("lintErrors")
        .map(|value| bool_config_value(value, "lintErrors", path))
        .transpose()?;

    patch.type_errors = value
        .get("typeErrors")
        .map(|value| bool_config_value(value, "typeErrors", path))
        .transpose()?;

    patch.globals = value
        .get("globals")
        .map(|value| json_string_array(value, "globals", path))
        .transpose()?;

    if let Some(aliases) = value.get("aliases").and_then(serde_json::Value::as_object) {
        for (alias, target) in aliases {
            let Some(target) = target.as_str() else {
                return Err(config_parse_error(
                    path,
                    format!("alias {alias} target must be a string"),
                ));
            };
            patch.aliases.push((alias.clone(), target.to_owned()));
        }
    }

    Ok(patch)
}

fn bool_config_value(value: &serde_json::Value, field: &str, path: &Path) -> ResolverResult<bool> {
    value
        .as_bool()
        .ok_or_else(|| config_parse_error(path, format!("{field} must be a boolean")))
}

fn json_string_array(
    value: &serde_json::Value,
    field: &str,
    path: &Path,
) -> ResolverResult<Vec<String>> {
    let Some(items) = value.as_array() else {
        return Err(config_parse_error(
            path,
            format!("{field} must be an array"),
        ));
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| config_parse_error(path, format!("{field} entries must be strings")))
        })
        .collect()
}

fn parse_config_mode(mode: &str, compat: bool, path: &Path) -> ResolverResult<Mode> {
    match mode {
        "nocheck" => Ok(Mode::NoCheck),
        "nonstrict" => Ok(Mode::Nonstrict),
        "strict" => Ok(Mode::Strict),
        "noinfer" if compat => Ok(Mode::NoCheck),
        _ => Err(config_parse_error(
            path,
            format!("bad language mode {mode:?}"),
        )),
    }
}

fn decode_config_luau_fields(body: &[Stat], path: &Path) -> ResolverResult<ConfigPatch> {
    let mut patch = ConfigPatch {
        mode: config_luau_field(body, "languagemode")
            .map(|value| string_expr_value(value, "languagemode", path))
            .transpose()?
            .map(|mode| parse_config_mode(mode, false, path))
            .transpose()?,
        lint_errors: config_luau_field(body, "linterrors")
            .map(|value| bool_expr_value(value, "linterrors", path))
            .transpose()?,
        type_errors: config_luau_field(body, "typeerrors")
            .map(|value| bool_expr_value(value, "typeerrors", path))
            .transpose()?,
        globals: config_luau_field(body, "globals")
            .map(|value| string_array_expr_value(value, "globals", path))
            .transpose()?,
        aliases: Vec::new(),
    };

    if let Some(aliases) = config_luau_aliases(body) {
        for item in aliases {
            let Some(alias) = item.key.as_ref().and_then(table_key_name) else {
                continue;
            };
            let Expr::String { value, .. } = &item.value else {
                return Err(config_parse_error(
                    path,
                    format!("alias {alias} target must be a string"),
                ));
            };
            patch.aliases.push((alias.to_owned(), value.clone()));
        }
    }

    Ok(patch)
}

fn config_luau_aliases(body: &[Stat]) -> Option<&[TableItem]> {
    match config_luau_field(body, "aliases")? {
        Expr::Table { items, .. } => Some(items),
        _ => None,
    }
}

fn config_luau_field<'a>(body: &'a [Stat], field: &str) -> Option<&'a Expr> {
    let returned = body.iter().rev().find_map(|stat| {
        let Stat::Return { list, .. } = stat else {
            return None;
        };
        list.first()
    })?;

    match returned {
        Expr::Table { items, .. } => field_from_config_table(items, field),
        Expr::Local { local, .. } => field_from_returned_local(body, local.name.as_str(), field),
        _ => None,
    }
}

/// Finds a field in the `luau` table inside a literal config table.
fn field_from_config_table<'a>(items: &'a [TableItem], field: &str) -> Option<&'a Expr> {
    let Expr::Table {
        items: luau_items, ..
    } = table_field(items, "luau")?
    else {
        return None;
    };
    table_field(luau_items, field)
}

/// Finds a `luau` field assigned to the returned config local.
fn field_from_returned_local<'a>(body: &'a [Stat], local: &str, field: &str) -> Option<&'a Expr> {
    assigned_expr(body, &[local, "luau", field])
        .or_else(|| {
            assigned_table_items(body, &[local, "luau"]).and_then(|luau| table_field(luau, field))
        })
        .or_else(|| {
            local_table_initializer(body, local)
                .and_then(|items| field_from_config_table(items, field))
        })
}

/// Finds the latest table assigned to a dotted/indexed expression path.
fn assigned_table_items<'a>(body: &'a [Stat], path: &[&str]) -> Option<&'a [TableItem]> {
    let Expr::Table { items, .. } = assigned_expr(body, path)? else {
        return None;
    };
    Some(items.as_slice())
}

/// Finds the latest expression assigned to a dotted/indexed expression path.
fn assigned_expr<'a>(body: &'a [Stat], path: &[&str]) -> Option<&'a Expr> {
    body.iter().rev().find_map(|stat| {
        let Stat::Assign { vars, values, .. } = stat else {
            return None;
        };
        vars.iter().zip(values).find_map(|(var, value)| {
            if !expr_path_matches(var, path) {
                return None;
            }
            Some(value)
        })
    })
}

/// Finds a local table initializer by binding name.
fn local_table_initializer<'a>(body: &'a [Stat], local: &str) -> Option<&'a [TableItem]> {
    body.iter().find_map(|stat| {
        let Stat::Local { vars, values, .. } = stat else {
            return None;
        };
        vars.iter().zip(values).find_map(|(var, value)| {
            if var.name.as_str() != local {
                return None;
            }
            let Expr::Table { items, .. } = value else {
                return None;
            };
            Some(items.as_slice())
        })
    })
}

/// Returns whether an expression names a config table path.
fn expr_path_matches(expr: &Expr, path: &[&str]) -> bool {
    let Some((last, parent)) = path.split_last() else {
        return false;
    };
    match expr {
        Expr::Local { local, .. } => parent.is_empty() && local.name.as_str() == *last,
        Expr::Global { name, .. } => parent.is_empty() && name.as_str() == *last,
        Expr::IndexName { expr, index, .. } => {
            index.as_str() == *last && expr_path_matches(expr, parent)
        }
        Expr::IndexExpr { expr, index, .. } => {
            let Expr::String { value, .. } = index.as_ref() else {
                return false;
            };
            value == *last && expr_path_matches(expr, parent)
        }
        Expr::Group { expr, .. } => expr_path_matches(expr, path),
        _ => false,
    }
}

/// Reads one config file and maps I/O errors into resolver diagnostics.
fn read_config_file(
    path: &Path,
    file: OpenedFile,
    max_read_bytes: usize,
) -> ResolverResult<String> {
    String::from_utf8(
        file.read_bounded(max_read_bytes)
            .map_err(|error| bounded_read_resolver_error(path, error))?,
    )
    .map_err(|error| bounded_read_resolver_error(path, BoundedReadError::InvalidUtf8(error)))
}

/// Builds a config or source I/O error for a path.
fn io_diagnostic(path: &Path, error: &io::Error) -> ResolverError {
    ResolverError::Io {
        path: path.to_path_buf(),
        detail: error.to_string(),
    }
}

#[derive(Debug)]
enum BoundedReadError {
    Io(io::Error),
    TooLarge { max_bytes: usize },
    InvalidUtf8(FromUtf8Error),
}

impl fmt::Display for BoundedReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::TooLarge { max_bytes } => formatter.write_str(&max_read_bytes_error(*max_bytes)),
            Self::InvalidUtf8(error) => write!(formatter, "file is not UTF-8: {error}"),
        }
    }
}

fn read_bounded_file(
    file: fs::File,
    length: u64,
    max_bytes: usize,
) -> Result<Vec<u8>, BoundedReadError> {
    if length > max_bytes as u64 {
        return Err(BoundedReadError::TooLarge { max_bytes });
    }

    let read_limit = max_bytes
        .checked_add(1)
        .map_or(u64::MAX, |limit| limit as u64);
    let mut reader = file.take(read_limit);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(BoundedReadError::Io)?;
    if bytes.len() > max_bytes {
        return Err(BoundedReadError::TooLarge { max_bytes });
    }
    Ok(bytes)
}

fn bounded_read_resolver_error(path: &Path, error: BoundedReadError) -> ResolverError {
    match error {
        BoundedReadError::Io(error) => io_diagnostic(path, &error),
        BoundedReadError::TooLarge { max_bytes } => ResolverError::Io {
            path: path.to_path_buf(),
            detail: max_read_bytes_error(max_bytes),
        },
        BoundedReadError::InvalidUtf8(error) => ResolverError::Io {
            path: path.to_path_buf(),
            detail: format!("file is not UTF-8: {error}"),
        },
    }
}

fn bounded_read_module_source_error(display_name: &str, error: BoundedReadError) -> SourceError {
    match error {
        BoundedReadError::Io(error) => {
            SourceError::other(format!("I/O error reading {display_name}: {error}"))
        }
        BoundedReadError::TooLarge { max_bytes } => SourceError::other(format!(
            "source {display_name} {}",
            max_read_bytes_error(max_bytes)
        )),
        BoundedReadError::InvalidUtf8(error) => {
            SourceError::other(format!("source {display_name} is not UTF-8: {error}"))
        }
    }
}

fn max_read_bytes_error(max_bytes: usize) -> String {
    format!("exceeds filesystem read limit of {max_bytes} bytes")
}

fn string_expr_value<'a>(expr: &'a Expr, field: &str, path: &Path) -> ResolverResult<&'a str> {
    let Expr::String { value, .. } = expr else {
        return Err(config_parse_error(
            path,
            format!("{field} must be a string"),
        ));
    };
    Ok(value)
}

fn bool_expr_value(expr: &Expr, field: &str, path: &Path) -> ResolverResult<bool> {
    let Expr::Bool { value, .. } = expr else {
        return Err(config_parse_error(
            path,
            format!("{field} must be a boolean"),
        ));
    };
    Ok(*value)
}

fn string_array_expr_value(expr: &Expr, field: &str, path: &Path) -> ResolverResult<Vec<String>> {
    let Expr::Table { items, .. } = expr else {
        return Err(config_parse_error(
            path,
            format!("{field} must be an array of strings"),
        ));
    };

    items
        .iter()
        .map(|item| {
            let Expr::String { value, .. } = &item.value else {
                return Err(config_parse_error(
                    path,
                    format!("{field} entries must be strings"),
                ));
            };
            Ok(value.clone())
        })
        .collect()
}

fn config_parse_error(path: &Path, detail: impl Into<String>) -> ResolverError {
    ResolverError::ConfigError {
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

/// Sets an alias found in a config file.
fn set_materialized_alias(
    config: &mut ModuleConfig,
    alias: &str,
    target: &str,
    path: &Path,
) -> ResolverResult<()> {
    if !is_valid_alias(alias) {
        return Err(ResolverError::InvalidAlias {
            alias: alias.to_owned(),
            source: path.to_path_buf(),
        });
    }
    config.add_alias(Alias {
        name: alias.to_owned(),
        target: target.to_owned(),
        origin: Some(Origin::File(path.to_path_buf())),
    });
    Ok(())
}

/// Returns a named field from a table constructor.
fn table_field<'a>(items: &'a [TableItem], field: &str) -> Option<&'a Expr> {
    items
        .iter()
        .find(|item| item.key.as_ref().and_then(table_key_name) == Some(field))
        .map(|item| &item.value)
}

/// Returns the string name for a table item key.
fn table_key_name(key: &Expr) -> Option<&str> {
    match key {
        Expr::String { value, .. } => Some(value),
        _ => None,
    }
}

#[cfg(any())]
mod tests;
