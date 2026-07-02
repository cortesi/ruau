//! Static analysis infrastructure above the parser.
//!
//! [`Frontend`] parses source graphs and reports require cycles.
//! [`SourceNode`] describes the graph topology. Static require helpers collect
//! direct string `require(...)` calls for tools that do not need a full graph.

mod frontend;
mod graph;
mod require_tracer;

pub mod resolve;

pub use frontend::{Frontend, FrontendStats, ParseGraphResult, RequireCycle, SourceModule};
use ruau_ast::parse::{Error, HotComment};
#[cfg(any())]
mod test_resolver;

#[cfg(any())]
/// Fixture-only resolver APIs used by upstream conformance tests and xtask
/// audits.
pub mod fixtures {
    pub use crate::{
        require_tracer::trace_requires,
        resolve::resolver::{FileResolver, InMemorySourceResolver, ReadyModuleSourceFiles},
        test_resolver::{
            FixtureRequireOptions, FixtureRequireResolver, RobloxResolver, child_module,
            fixture_dirs, game_get_service, parent_module, resolve_fixture_require_expr,
            resolve_roblox_module_expr, script_module, unresolved_read_source,
            upstream_fixture_root,
        },
    };
}

pub use graph::SourceNode;
pub use require_tracer::{
    RequireListEntry, RequireResolution, RequireTraceResult, StaticRequireRequest,
    static_require_requests, static_require_requests_with_locations,
};

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

#[cfg(any())]
mod tests;
