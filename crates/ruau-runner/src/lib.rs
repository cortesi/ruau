//! Bounded request execution for untrusted source.
//!
//! [`Runner`] owns the native request lifecycle for a server-style embedder:
//! ingress admission, front-door parsing/checking/compilation, lane admission,
//! VM execution, result rendering, and aggregate tenant accounting. It builds a
//! fresh sandboxed VM for each accepted request and copies results into owned
//! [`ResultValue`]s before the VM is dropped.
//!
//! # Limits map
//!
//! [`IngressLimits`] cap accepted requests before parser/checker/compiler work
//! starts. [`FrontDoorLimits`] cap product size during parse, check, and
//! compile. [`AdmissionLimits`] cap work admitted to the lane pool and ready
//! queue. [`AggregateResourceLimits`] cap per-tenant totals recorded from
//! finished request reports. The per-request [`Budget`] carries the wall-clock
//! deadline and cancellation token; gas and memory ceilings come from the
//! runner's VM [`ruau_vm::Limits`].
//!
//! Configure the shared [`ruau_surface::Surface`], VM limits, admission caps,
//! and compiler options with [`Builder`].

mod admission;
mod budget;
mod builder;
mod front_door;
mod lanes;
mod pipeline;
mod render;
mod types;

#[cfg(any())]
mod accounting_tests;
#[cfg(any())]
mod tests;

// `InMemorySource` and `CompileOptions` live in `ruau-source` /
// `ruau-bytecode`; session/runtime types keep their canonical homes there.
pub use budget::Budget;
pub use builder::{BuildError, Builder};
pub use pipeline::Runner;
pub use types::{
    AggregateResourceLimit, AggregateResourceLimits, BudgetError, FailureCategory, FrontDoorLimit,
    FrontDoorLimits, FrontDoorStage, IngressLimits, IngressScope, Request, RequestError,
    RequestMetrics, RequestOutcome, RequestReport, RequestReportMetadata, RequestReportOutcome,
    ResultValue, StopReason, TenantId, TenantResourceTotals,
};

pub use crate::lanes::{
    AdmissionDecision, AdmissionLimits, AdmissionPolicy, AdmissionSnapshot, DefaultAdmissionPolicy,
    LaneMetrics,
};
