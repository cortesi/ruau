//! Bounded request execution for untrusted source.
//!
//! [`Executor`] owns the native request lifecycle for a server-style embedder:
//! ingress admission, preflight parsing/checking/compilation, lane admission,
//! VM execution, result rendering, and aggregate tenant accounting. It builds a
//! fresh sandboxed VM for each accepted request and copies results into owned
//! [`ValueSnapshot`]s before the VM is dropped.
//!
//! # Limits map
//!
//! [`IngressLimits`] cap accepted requests before parser/checker/compiler work
//! starts. [`PreflightLimits`] cap product size during parse, check, and
//! compile. [`AdmissionLimits`] cap work admitted to the lane pool and ready
//! queue. [`AggregateResourceLimits`] cap per-tenant totals recorded from
//! finished request reports. The per-request [`RunControl`] carries the wall-clock
//! deadline and cancellation token; gas and memory ceilings come from the
//! executor's VM [`ruau_vm::Limits`].
//!
//! Configure the shared [`ruau_surface::Surface`], VM limits, admission caps,
//! and compiler options with [`Builder`].

mod admission;
mod builder;
mod lanes;
mod pipeline;
mod preflight;
mod render;
mod run_control;
mod types;

#[cfg(any())]
mod accounting_tests;
#[cfg(any())]
mod tests;

// `InMemorySource` and `CompileOptions` live in `ruau-source` /
// `ruau-bytecode`; session/runtime types keep their canonical homes there.
pub use builder::{BuildError, Builder};
pub use pipeline::Executor;
pub use run_control::RunControl;
pub use types::{
    AggregateResourceLimit, AggregateResourceLimits, FailureCategory, IngressLimits, IngressScope,
    PreflightLimit, PreflightLimits, PreflightStage, Request, RequestError, RequestMetrics,
    RunControlError, RunMetadata, RunOutcome, RunReport, TenantId, TenantResourceTotals,
};

pub use crate::lanes::{
    AdmissionDecision, AdmissionLimits, AdmissionPolicy, AdmissionSnapshot, DefaultAdmissionPolicy,
    LaneMetrics, LaneStartupError,
};
