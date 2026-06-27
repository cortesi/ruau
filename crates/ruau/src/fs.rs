//! Filesystem-backed module sources and config materializers.

use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use ruau_analysis::resolve::{
    AnalysisMode, ModuleInfo, ResolverError, ResolverResult,
    config::{Alias, AnalysisConfig, Origin, Resolver},
    is_valid_alias, resolve_requested_module_name,
};
use ruau_ast::{
    parse::parse_file,
    syntax::{Expr, Stat, TableItem},
};
use ruau_source::{
    ModuleId, ModuleName, ModuleSource, ModuleSourceError, ModuleSourceFuture, SourceMetadata,
    ready,
};

/// Default byte cap for one filesystem-backed source or config file.
pub const DEFAULT_MAX_READ_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedSource {
    module: ModuleName,
    path: PathBuf,
}

#[derive(Clone, Debug)]
struct FilesystemSourceResolver {
    root: PathBuf,
    max_read_bytes: usize,
}

impl FilesystemSourceResolver {
    fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
        }
    }

    #[cfg(any())]
    fn with_max_read_bytes(mut self, max_read_bytes: usize) -> Self {
        self.max_read_bytes = max_read_bytes;
        self
    }

    fn resolve_module_name(&self, name: &ModuleName) -> ResolverResult<ResolvedSource> {
        let module_name = ModuleName::normalize(name.as_str());
        if module_name_escapes_root(&module_name) {
            return Err(ResolverError::ModuleEscapesRoot {
                module: ModuleName::new(module_name),
            });
        }
        if is_direct_init_module(&module_name) {
            return Err(ResolverError::InitFileRequiredDirectly {
                module: name.clone(),
            });
        }

        let base = self.root.join(module_name_to_path(&module_name));
        let mut candidates = Vec::new();

        for path in source_file_candidates(&base) {
            if path.is_file() {
                candidates.push(resolved_module_source(&module_name, path));
            }
        }

        for path in init_file_candidates(&base) {
            if path.is_file() {
                candidates.push(resolved_module_source(&module_name, path));
            }
        }

        match candidates.len() {
            0 => Err(ResolverError::MissingModule {
                module: name.clone(),
                searched: Some(base),
            }),
            1 => Ok(candidates.remove(0)),
            _ => Err(ResolverError::AmbiguousModule {
                module: name.clone(),
                searched: base,
            }),
        }
    }

    fn resolve_request(
        &self,
        context: Option<&ModuleInfo>,
        request: &str,
    ) -> ResolverResult<ModuleInfo> {
        let requested = resolve_requested_module_name(context, request)?;
        let resolved = self.resolve_module_name(&ModuleName::new(requested))?;
        Ok(ModuleInfo::new(resolved.module))
    }

    #[cfg(any())]
    fn read_module_source(&self, name: &ModuleName) -> ResolverResult<String> {
        let resolved = self.resolve_module_name(name)?;
        let source = read_bounded_utf8(&resolved.path, self.max_read_bytes)
            .map_err(|error| bounded_read_resolver_error(&resolved.path, error))?;
        Ok(source)
    }
}

/// Filesystem-backed [`ModuleSource`] adapter.
///
/// Reads are blocking and immediately ready. Portable module names cannot
/// escape `root`.
#[derive(Clone, Debug)]
pub struct FilesystemSource {
    resolver: FilesystemSourceResolver,
    epoch: FilesystemEpoch,
}

impl FilesystemSource {
    /// Creates a filesystem module source rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_epoch(root, 0)
    }

    /// Creates a filesystem module source with cache epoch `epoch`.
    #[must_use]
    pub fn with_epoch(root: impl Into<PathBuf>, epoch: u64) -> Self {
        Self::with_epoch_handle(root, FilesystemEpoch::new(epoch))
    }

    /// Creates a filesystem module source with a shared epoch handle.
    #[must_use]
    pub fn with_epoch_handle(root: impl Into<PathBuf>, epoch: FilesystemEpoch) -> Self {
        Self {
            resolver: FilesystemSourceResolver::new(root),
            epoch,
        }
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
    pub fn epoch_handle(&self) -> FilesystemEpoch {
        self.epoch.clone()
    }

    fn module_name_from_id(id: &ModuleId) -> Result<ModuleName, ModuleSourceError> {
        ModuleName::from_id(id)
    }

    fn display_name(&self, resolved: &ResolvedSource) -> String {
        resolved
            .path
            .strip_prefix(&self.resolver.root)
            .unwrap_or(&resolved.path)
            .to_string_lossy()
            .into_owned()
    }
}

impl ModuleSource for FilesystemSource {
    fn resolve(
        &self,
        requester: Option<&ModuleId>,
        request: &[u8],
    ) -> ModuleSourceFuture<ModuleId> {
        let result = (|| {
            let request = std::str::from_utf8(request).map_err(|error| {
                ModuleSourceError::other(format!("module request is not UTF-8: {error}"))
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

    fn read(&self, id: &ModuleId) -> ModuleSourceFuture<Vec<u8>> {
        let result = (|| {
            let name = Self::module_name_from_id(id)?;
            let resolved = self
                .resolver
                .resolve_module_name(&name)
                .map_err(module_source_error_from_resolver)?;
            let bytes = read_bounded_bytes(&resolved.path, self.resolver.max_read_bytes).map_err(
                |error| {
                    let display_name = self.display_name(&resolved);
                    bounded_read_module_source_error(&display_name, error)
                },
            )?;
            std::str::from_utf8(&bytes).map_err(|error| {
                let display_name = self.display_name(&resolved);
                ModuleSourceError::other(format!("source {display_name} is not UTF-8: {error}"))
            })?;
            Ok(bytes)
        })();
        ready(result)
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
        self.epoch.value()
    }
}

/// Cloneable cache-invalidation epoch for a filesystem module source.
///
/// Advance it when source contents change.
#[derive(Clone, Debug)]
pub struct FilesystemEpoch {
    value: Arc<AtomicU64>,
}

impl FilesystemEpoch {
    /// Creates an epoch handle initialized to `epoch`.
    #[must_use]
    pub fn new(epoch: u64) -> Self {
        Self {
            value: Arc::new(AtomicU64::new(epoch)),
        }
    }

    /// Returns the current epoch value.
    #[must_use]
    pub fn value(&self) -> u64 {
        self.value.load(Ordering::SeqCst)
    }

    /// Sets the current epoch value.
    pub fn set(&self, epoch: u64) {
        self.value.store(epoch, Ordering::SeqCst);
    }

    /// Advances the epoch and returns the new value.
    pub fn bump(&self) -> u64 {
        self.value.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }
}

impl Default for FilesystemEpoch {
    fn default() -> Self {
        Self::new(0)
    }
}

fn module_source_error_from_resolver(error: ResolverError) -> ModuleSourceError {
    match error {
        ResolverError::MissingModule { module, .. } => ModuleSourceError::MissingModule {
            id: ModuleId::canonicalized(module.as_str()),
        },
        ResolverError::UnresolvableRelativeRequest { request } => {
            ModuleSourceError::UnresolvableRelativeRequest {
                request: request.into_bytes(),
            }
        }
        ResolverError::ModuleEscapesRoot { module } => {
            ModuleSourceError::other(format!("module `{module}` escapes filesystem root"))
        }
        other => ModuleSourceError::other(other.to_string()),
    }
}

/// Filesystem-backed config materializer.
///
/// Portable module names cannot escape `root`.
#[derive(Clone, Debug)]
pub struct FilesystemResolver {
    /// Root directory used to interpret portable module names.
    root: PathBuf,
    max_read_bytes: usize,
}

impl FilesystemResolver {
    /// Creates a filesystem config resolver rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
        }
    }

    /// Returns the resolver root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sets the maximum bytes read for one config file.
    ///
    /// Files over this cap are rejected before allocating their full contents.
    #[must_use]
    pub fn with_max_read_bytes(mut self, max_read_bytes: usize) -> Self {
        self.max_read_bytes = max_read_bytes;
        self
    }

    /// Materializes the effective config for `name`.
    pub fn materialize_config_for_module(
        &self,
        name: &ModuleName,
    ) -> ResolverResult<AnalysisConfig> {
        let module_name = ModuleName::normalize(name.as_str());
        if module_name_escapes_root(&module_name) {
            return Err(ResolverError::ModuleEscapesRoot {
                module: ModuleName::new(module_name),
            });
        }
        let module_parent = ModuleName::from(module_name.as_str())
            .parent()
            .as_str()
            .to_owned();
        let mut config = AnalysisConfig::new();

        for directory in config_search_directories(&self.root, &module_parent) {
            if let Some(directory_config) =
                load_config_from_directory(&directory, self.max_read_bytes)?
            {
                config = config.merged_with(&directory_config);
            }
        }

        Ok(config)
    }
}

impl Resolver for FilesystemResolver {
    fn config_for_module(&self, name: &ModuleName) -> ResolverResult<AnalysisConfig> {
        self.materialize_config_for_module(name)
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
fn resolved_module_source(module_name: &str, path: PathBuf) -> ResolvedSource {
    ResolvedSource {
        module: ModuleName::new(module_name),
        path,
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
fn config_search_directories(root: &Path, module_parent: &str) -> Vec<PathBuf> {
    let mut directories = vec![root.to_path_buf()];
    let mut current = PathBuf::from(root);
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
    directory: &Path,
    max_read_bytes: usize,
) -> ResolverResult<Option<AnalysisConfig>> {
    let luaurc = directory.join(".luaurc");
    let config_luau = directory.join(".config.luau");
    let luaurc_exists = luaurc.is_file();
    let config_luau_exists = config_luau.is_file();

    match (luaurc_exists, config_luau_exists) {
        (false, false) => Ok(None),
        (true, true) => Err(ResolverError::ConfigAmbiguity {
            directory: directory.to_path_buf(),
        }),
        (true, false) => parse_luaurc_file(&luaurc, max_read_bytes).map(Some),
        (false, true) => parse_config_luau_file(&config_luau, max_read_bytes).map(Some),
    }
}

/// Parses a `.luaurc` file into portable config.
fn parse_luaurc_file(path: &Path, max_read_bytes: usize) -> ResolverResult<AnalysisConfig> {
    let contents = read_config_file(path, max_read_bytes)?;
    let contents = normalize_luaurc_json(&contents);
    let value: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|error| config_parse_error(path, error.to_string()))?;
    let mut config = AnalysisConfig::new();

    apply_luaurc_config(&mut config, &value, path)?;

    Ok(config)
}

/// Parses a `.config.luau` file into portable config.
fn parse_config_luau_file(path: &Path, max_read_bytes: usize) -> ResolverResult<AnalysisConfig> {
    let contents = read_config_file(path, max_read_bytes)?;
    let result = parse_file(&contents);
    if let Some(error) = result.errors.first() {
        let line = error.location.begin.line + 1;
        let column = error.location.begin.column + 1;
        return Err(config_parse_error(
            path,
            format!("{} (at line {}, column {})", error.message, line, column),
        ));
    }

    let mut config = AnalysisConfig::new();
    let Some(Stat::Block { body, .. }) = result.root.as_ref() else {
        return Ok(config);
    };
    apply_config_luau_fields(&mut config, body, path)?;

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

fn apply_luaurc_config(
    config: &mut AnalysisConfig,
    value: &serde_json::Value,
    path: &Path,
) -> ResolverResult<()> {
    if let Some(mode) = value
        .get("languageMode")
        .and_then(serde_json::Value::as_str)
        .map(|mode| parse_config_mode(mode, false, path))
        .transpose()?
    {
        config.set_mode(mode);
    }

    if let Some(language) = value.get("language").and_then(serde_json::Value::as_object)
        && let Some(mode) = language
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .map(|mode| parse_config_mode(mode, true, path))
            .transpose()?
    {
        config.set_mode(mode);
    }

    if let Some(lint_errors) = value
        .get("lintErrors")
        .map(|value| bool_config_value(value, "lintErrors", path))
        .transpose()?
    {
        config.set_lint_errors(lint_errors);
    }

    if let Some(type_errors) = value
        .get("typeErrors")
        .map(|value| bool_config_value(value, "typeErrors", path))
        .transpose()?
    {
        config.set_type_errors(type_errors);
    }

    if let Some(globals) = value
        .get("globals")
        .map(|value| json_string_array(value, "globals", path))
        .transpose()?
    {
        config.set_globals(globals);
    }

    if let Some(aliases) = value.get("aliases").and_then(serde_json::Value::as_object) {
        for (alias, target) in aliases {
            let Some(target) = target.as_str() else {
                return Err(config_parse_error(
                    path,
                    format!("alias {alias} target must be a string"),
                ));
            };
            set_materialized_alias(config, alias, target, path)?;
        }
    }

    Ok(())
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

fn parse_config_mode(mode: &str, compat: bool, path: &Path) -> ResolverResult<AnalysisMode> {
    match mode {
        "nocheck" => Ok(AnalysisMode::NoCheck),
        "nonstrict" => Ok(AnalysisMode::Nonstrict),
        "strict" => Ok(AnalysisMode::Strict),
        "noinfer" if compat => Ok(AnalysisMode::NoCheck),
        _ => Err(config_parse_error(
            path,
            format!("bad language mode {mode:?}"),
        )),
    }
}

fn apply_config_luau_fields(
    config: &mut AnalysisConfig,
    body: &[Stat],
    path: &Path,
) -> ResolverResult<()> {
    if let Some(mode) = config_luau_field(body, "languagemode")
        .map(|value| string_expr_value(value, "languagemode", path))
        .transpose()?
        .map(|mode| parse_config_mode(mode, false, path))
        .transpose()?
    {
        config.set_mode(mode);
    }

    if let Some(lint_errors) = config_luau_field(body, "linterrors")
        .map(|value| bool_expr_value(value, "linterrors", path))
        .transpose()?
    {
        config.set_lint_errors(lint_errors);
    }

    if let Some(type_errors) = config_luau_field(body, "typeerrors")
        .map(|value| bool_expr_value(value, "typeerrors", path))
        .transpose()?
    {
        config.set_type_errors(type_errors);
    }

    if let Some(globals) = config_luau_field(body, "globals")
        .map(|value| string_array_expr_value(value, "globals", path))
        .transpose()?
    {
        config.set_globals(globals);
    }

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
            set_materialized_alias(config, alias, value, path)?;
        }
    }

    Ok(())
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
fn read_config_file(path: &Path, max_read_bytes: usize) -> ResolverResult<String> {
    read_bounded_utf8(path, max_read_bytes)
        .map_err(|error| bounded_read_resolver_error(path, error))
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
    InvalidUtf8(std::string::FromUtf8Error),
}

fn read_bounded_utf8(path: &Path, max_bytes: usize) -> Result<String, BoundedReadError> {
    let bytes = read_bounded_bytes(path, max_bytes)?;
    String::from_utf8(bytes).map_err(BoundedReadError::InvalidUtf8)
}

fn read_bounded_bytes(path: &Path, max_bytes: usize) -> Result<Vec<u8>, BoundedReadError> {
    let file = fs::File::open(path).map_err(BoundedReadError::Io)?;
    if file.metadata().map_err(BoundedReadError::Io)?.len() > max_bytes as u64 {
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

fn bounded_read_module_source_error(
    display_name: &str,
    error: BoundedReadError,
) -> ModuleSourceError {
    match error {
        BoundedReadError::Io(error) => {
            ModuleSourceError::other(format!("I/O error reading {display_name}: {error}"))
        }
        BoundedReadError::TooLarge { max_bytes } => ModuleSourceError::other(format!(
            "source {display_name} {}",
            max_read_bytes_error(max_bytes)
        )),
        BoundedReadError::InvalidUtf8(error) => {
            ModuleSourceError::other(format!("source {display_name} is not UTF-8: {error}"))
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
    config: &mut AnalysisConfig,
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
