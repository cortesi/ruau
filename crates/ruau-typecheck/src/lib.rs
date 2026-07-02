//! Luau type checking and type inference.
//!
//! Provides checker and frontend entry points, diagnostics, schema extraction,
//! read-only type views, and fixture-gated source queries.
//!
//! # Entry points
//!
//! [`Checker::check_source`] checks one standalone source string and returns a
//! [`CheckedModule`]. [`frontend::GraphChecker::check_graph`] checks a
//! [`ruau_source::ModuleSource`] root plus its statically reachable
//! dependencies and returns graph diagnostics. [`schema::extract_module`] and
//! [`schema::extract_frontend`] convert checked exports into declaration-schema
//! data. [`views::TypeView`] provides read-only inspection of public type
//! handles without exposing the internal arena representation.
//!
//! ```no_run
//! use ruau_typecheck::Checker;
//!
//! let mut checker = Checker::new();
//! let checked = checker.check_source("--!strict\nlocal n: number = 1\nreturn n");
//! assert!(checked.diagnostics().is_empty());
//! ```

pub(crate) mod annotation;
pub(crate) mod ast_util;
pub mod builtins;
pub(crate) mod call_pack;
mod checker;
pub(crate) mod constraints;
pub(crate) mod dfg;
pub(crate) mod diagnostic_selection;
mod diagnostics;
pub(crate) mod fastmap;
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
#[cfg(any())]
pub mod queries;
#[cfg(not(any()))]
#[allow(dead_code)]
pub(crate) mod queries;
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

pub use checker::{
    CheckedModule, Checker, Config, ConformanceCheck, ConformanceFingerprint, ExportedType,
    ExportedTypeKind, GenerationConfig, GenericPackParameter, GenericParameter,
    ImportedModuleSummary, ModuleExports,
};
pub use diagnostics::{
    ArityCounts, Diagnostic, DiagnosticCategory, DiagnosticLocation, DiagnosticPosition,
    DiagnosticView, Diagnostics, GenericCountMismatch, GenericParameterKind, GraphDiagnostics,
    ModuleDiagnostic, ModuleDiagnosticView, OneBasedDiagnosticLocation, OneBasedDiagnosticPosition,
    Payload, PropertyAccess, ReasonPath, ReasonPathEntry, RecommendedArgument, Severity,
    SubtypeContext, SuppressionMetadata, UnionPropertyMissing,
};
#[cfg(any())]
pub use fixtures::{FixtureAudit, FixtureAuditFailure, audit_upstream_fixtures};

/// Version information for the Ruau type checker crate.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(any())]
mod tests;
