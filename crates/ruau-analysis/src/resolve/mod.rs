//! Parser-dependent source analysis support, grouped by purpose:
//!
//! - [`config`] — [`AnalysisConfig`](config::AnalysisConfig), aliases, and the
//!   [`Resolver`](config::Resolver) trait with adapters.
//! - [`ResolverError`] and [`ResolverResult`] — diagnostics shared by config
//!   resolution and source graph loading.
//!
//! Analysis [`crate::AnalysisMode`] and [`crate::effective_mode`] live at the
//! crate root because
//! they are shared by both the frontend and the standalone checker.

pub mod config;
#[cfg(any())]
pub(crate) mod path;
pub(crate) mod resolver;

pub use resolver::{
    ModuleInfo, ResolverError, ResolverResult, SourceCode, resolve_requested_module_name,
};

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
