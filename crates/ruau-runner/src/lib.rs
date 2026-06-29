//! Bounded request execution for untrusted source.
//!
//! [`Runner`] checks, compiles, sandboxes, runs, renders, and drops a
//! fresh VM for each request. Configure the shared surface,
//! limits, admission caps, and compiler options with [`Builder`].
//! Results are copied into owned [`ResultValue`]s before the VM is dropped.

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
pub use builder::Builder;
pub use pipeline::Runner;
pub use types::{
    AggregateResourceLimit, AggregateResourceLimits, BudgetError, FailureCategory, FrontDoorLimit,
    FrontDoorLimits, FrontDoorStage, IngressLimits, IngressScope, Request, RequestError,
    RequestMetrics, RequestOutcome, RequestReport, RequestReportMetadata, RequestReportOutcome,
    ResultValue, StopReason, TenantId, TenantResourceTotals,
};

pub use crate::lanes::{
    AdmissionDecision, AdmissionLimits, AdmissionPolicy, AdmissionSnapshot, LaneMetrics,
};
