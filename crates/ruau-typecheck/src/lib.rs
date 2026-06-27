//! Luau type checking and type inference.
//!
//! Provides checker and frontend entry points, diagnostics, schema extraction,
//! read-only type views, and source queries.

// The solver still passes rich representation types between internal modules.
// Keep those crate-visible helpers out of the ordinary public API.
#![allow(private_interfaces, unnameable_types)]
pub(crate) mod annotation;
pub(crate) mod ast_util;
pub mod builtins;
pub(crate) mod call_pack;
pub mod checker;
pub(crate) mod constraints;
pub(crate) mod dfg;
pub mod diagnostic;
pub(crate) mod diagnostic_selection;
#[cfg(any())]
mod fixtures;
pub mod frontend;
pub(crate) mod generalize;
pub(crate) mod generation;
pub(crate) mod generic_alias;
pub(crate) mod interface_snapshot;
pub(crate) mod magic_types;
pub(crate) mod member_access;
pub(crate) mod normalize;
pub(crate) mod overload;
pub(crate) mod post_solve;
pub mod queries;
pub(crate) mod query_surface;
pub mod schema;
pub(crate) mod scopes;
pub(crate) mod subtype;
#[cfg(any())]
pub(crate) mod test_context;
pub(crate) mod type_function;
pub(crate) mod type_graph;
#[cfg(any())]
pub mod type_match;
#[cfg(any())]
pub mod typeinfer_fixtures;
pub mod types;
pub(crate) mod unify;
pub mod views;

#[cfg(any())]
pub use fixtures::{FixtureAudit, FixtureAuditFailure, audit_upstream_fixtures};

/// Version information for the Ruau type checker crate.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(any())]
mod tests;
