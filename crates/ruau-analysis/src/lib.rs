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

#[cfg(any())]
mod tests;
