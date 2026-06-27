//! Parser-dependent source analysis support, grouped by purpose:
//!
//! - [`config`] — [`AnalysisConfig`](config::AnalysisConfig), aliases, and the
//!   [`Resolver`](config::Resolver) trait with adapters.
//! - [`ResolverError`] and [`ResolverResult`] — diagnostics shared by config
//!   resolution and source graph loading.
//!
//! Analysis [`AnalysisMode`] and [`effective_mode`] live at this root because
//! they are shared by both the frontend and the standalone checker.

pub mod config;
#[cfg(any())]
pub(crate) mod path;
pub(crate) mod resolver;

pub use resolver::{
    ModuleInfo, ResolverError, ResolverResult, SourceCode, resolve_requested_module_name,
};
use ruau_ast::parse::{Error, HotComment};

/// Source analysis mode from header hot comments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnalysisMode {
    /// Skip type checking.
    NoCheck,
    /// Nonstrict analysis.
    Nonstrict,
    /// Strict analysis.
    Strict,
}

impl AnalysisMode {
    /// Parses the analysis mode from header hot comments.
    #[must_use]
    pub fn from_hot_comments(hot_comments: &[HotComment]) -> Option<Self> {
        hot_comments
            .iter()
            .filter(|comment| comment.header)
            .find_map(|comment| match comment.content.as_str() {
                "nocheck" => Some(Self::NoCheck),
                "nonstrict" => Some(Self::Nonstrict),
                "strict" => Some(Self::Strict),
                _ => None,
            })
    }
}

/// Returns the effective mode for a parsed module: [`AnalysisMode::NoCheck`] when
/// parsing failed, otherwise the header mode, falling back to `config_mode`.
#[must_use]
pub fn effective_mode(
    parse_errors: &[Error],
    hot_comments: &[HotComment],
    config_mode: Option<AnalysisMode>,
) -> Option<AnalysisMode> {
    if !parse_errors.is_empty() {
        return Some(AnalysisMode::NoCheck);
    }
    AnalysisMode::from_hot_comments(hot_comments).or(config_mode)
}

/// Returns whether an alias matches upstream's accepted alias spelling.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    if alias.is_empty()
        || matches!(alias, "." | "..")
        || alias.contains('/')
        || alias.contains('\\')
    {
        return false;
    }

    alias.char_indices().all(|(index, character)| {
        (index == 0 && character == '@')
            || character.is_ascii_alphanumeric()
            || matches!(character, '-' | '_' | '.')
    })
}

#[cfg(any())]
mod tests;
