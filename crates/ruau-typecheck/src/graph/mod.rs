//! Static analysis infrastructure above the parser.
//!
//! [`Frontend`] parses source graphs and reports require cycles.
//! [`SourceNode`] describes the graph topology. Static require helpers collect
//! direct string `require(...)` calls for tools that do not need a full graph.

mod frontend;
#[allow(clippy::module_inception)]
mod graph;
mod require_tracer;

pub mod resolve;

#[cfg(any())]
pub use frontend::{Frontend, FrontendStats, ParseGraphResult, RequireCycle, SourceModule};
#[cfg(not(any()))]
pub use frontend::{Frontend, ParseGraphResult, RequireCycle, SourceModule};
use ruau_syntax::parse::{Error, HotComment};
#[cfg(any())]
mod test_resolver;

#[cfg(any())]
/// Fixture-only resolver APIs used by upstream conformance tests and xtask
/// audits.
pub mod fixtures {
    pub use super::{
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

#[cfg(any())]
pub use graph::SourceNode;
#[cfg(not(any()))]
pub use require_tracer::RequireTraceResult;
#[cfg(any())]
pub use require_tracer::{RequireListEntry, RequireResolution, RequireTraceResult};
pub use require_tracer::{
    RequireScan, StaticRequireRequest, scan_requires, static_require_requests,
    static_require_requests_with_locations,
};

/// Source analysis mode from header hot comments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Skip type checking.
    NoCheck,
    /// Nonstrict analysis.
    Nonstrict,
    /// Strict analysis.
    Strict,
}

impl Mode {
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

/// Returns the effective mode for a parsed module: [`Mode::NoCheck`] when
/// parsing failed, otherwise the header mode, falling back to `config_mode`.
#[must_use]
pub fn effective_mode(
    parse_errors: &[Error],
    hot_comments: &[HotComment],
    config_mode: Option<Mode>,
) -> Option<Mode> {
    if !parse_errors.is_empty() {
        return Some(Mode::NoCheck);
    }
    Mode::from_hot_comments(hot_comments).or(config_mode)
}

#[cfg(any())]
mod tests;
