//! Resolver config (aliases + checker knobs) and `Resolver` adapters.

use std::{borrow::Cow, collections::BTreeMap, path::PathBuf};

use ruau_source::ModuleName;

use super::resolver::ResolverResult;
use crate::graph::Mode;

/// Resolver config used by static require resolution.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleConfig {
    /// Resolver aliases, keyed by normalized alias name.
    aliases: BTreeMap<String, Alias>,
    /// Default checker mode supplied by config, when present.
    mode: Option<Mode>,
    /// Whether lint diagnostics should be reported as errors.
    lint_errors: Option<bool>,
    /// Whether type diagnostics should be reported as errors.
    type_errors: Option<bool>,
    /// Additional globals supplied by config, in source order.
    globals: Option<Vec<String>>,
}

impl ModuleConfig {
    /// Creates empty config.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns all aliases, keyed by normalized name.
    #[must_use]
    pub fn aliases(&self) -> &BTreeMap<String, Alias> {
        &self.aliases
    }

    /// Returns one alias, looked up case-insensitively.
    #[must_use]
    pub fn alias(&self, alias: &str) -> Option<&Alias> {
        self.aliases.get(&normalize_alias(alias))
    }

    /// Returns the configured checker mode, if one was supplied.
    #[must_use]
    pub const fn mode(&self) -> Option<Mode> {
        self.mode
    }

    /// Sets the configured checker mode.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = Some(mode);
    }

    /// Returns whether lint diagnostics should be reported as errors.
    #[must_use]
    pub fn lint_errors(&self) -> bool {
        self.lint_errors.unwrap_or(false)
    }

    /// Sets whether lint diagnostics should be reported as errors.
    pub fn set_lint_errors(&mut self, lint_errors: bool) {
        self.lint_errors = Some(lint_errors);
    }

    /// Returns whether type diagnostics should be reported as errors.
    #[must_use]
    pub fn type_errors(&self) -> bool {
        self.type_errors.unwrap_or(true)
    }

    /// Sets whether type diagnostics should be reported as errors.
    pub fn set_type_errors(&mut self, type_errors: bool) {
        self.type_errors = Some(type_errors);
    }

    /// Returns configured extra globals.
    #[must_use]
    pub fn globals(&self) -> &[String] {
        self.globals.as_deref().unwrap_or(&[])
    }

    /// Replaces configured extra globals.
    pub fn set_globals(&mut self, globals: Vec<String>) {
        self.globals = Some(globals);
    }

    /// Adds a resolver alias, replacing any existing alias with the same
    /// case-insensitive name.
    pub fn add_alias(&mut self, alias: Alias) {
        self.aliases.insert(normalize_alias(&alias.name), alias);
    }

    /// Returns config with `overrides` layered over this config.
    ///
    /// Aliases and explicitly-set knobs from `overrides` take precedence;
    /// values left unset on `overrides` keep this config's values.
    #[must_use]
    pub fn merged_with(&self, overrides: &Self) -> Self {
        let mut merged = self.clone();
        merged.aliases.extend(overrides.aliases.clone());
        merge_override(&mut merged.mode, overrides.mode);
        merge_override(&mut merged.lint_errors, overrides.lint_errors);
        merge_override(&mut merged.type_errors, overrides.type_errors);
        merge_override(&mut merged.globals, overrides.globals.clone());
        merged
    }
}

/// Resolver alias entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Alias {
    /// Alias spelling as it appeared in the source config. Looked up
    /// case-insensitively; this preserves the original casing for diagnostics.
    pub name: String,
    /// Alias target.
    pub target: String,
    /// Optional materialization origin.
    pub origin: Option<Origin>,
}

/// Origin of a materialized config value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Origin {
    /// Materialized from a config file on disk.
    File(PathBuf),
    /// Materialized from a virtual or in-memory source.
    Virtual(String),
}

impl Origin {
    /// Returns the origin as a human-readable label.
    #[must_use]
    pub fn label(&self) -> Cow<'_, str> {
        match self {
            Self::File(path) => path.to_string_lossy(),
            Self::Virtual(label) => Cow::Borrowed(label),
        }
    }
}

/// Provides effective config for a module.
pub trait Resolver: Sync {
    /// Returns effective config for `name`, reporting why materialization failed.
    fn config_for_module(&self, name: &ModuleName) -> ResolverResult<ModuleConfig>;
}

/// Empty config resolver.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyResolver;

impl Resolver for EmptyResolver {
    fn config_for_module(&self, _name: &ModuleName) -> ResolverResult<ModuleConfig> {
        Ok(ModuleConfig::new())
    }
}

/// In-memory config resolver for virtual sources and tests.
#[derive(Clone, Debug, Default)]
pub struct InMemoryResolver {
    /// Module-specific configs.
    configs: BTreeMap<ModuleName, ModuleConfig>,
    /// Fallback config used when no module-specific config exists.
    default: ModuleConfig,
}

impl InMemoryResolver {
    /// Creates an empty resolver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets fallback config.
    pub fn set_default(&mut self, config: ModuleConfig) {
        self.default = config;
    }

    /// Sets config for a module.
    pub fn insert(&mut self, name: impl Into<ModuleName>, config: ModuleConfig) {
        self.configs.insert(name.into(), config);
    }
}

impl Resolver for InMemoryResolver {
    fn config_for_module(&self, name: &ModuleName) -> ResolverResult<ModuleConfig> {
        Ok(self.configs.get(name).unwrap_or(&self.default).clone())
    }
}

/// Normalizes an alias key.
fn normalize_alias(alias: &str) -> String {
    alias.to_ascii_lowercase()
}

fn merge_override<T>(target: &mut Option<T>, override_value: Option<T>) {
    if let Some(value) = override_value {
        *target = Some(value);
    }
}
