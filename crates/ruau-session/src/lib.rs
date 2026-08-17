//! Target-neutral retained Ruau sessions.

mod retained;

pub use retained::{
    FunctionHandle, Handle, HandleKind, Invalidation, InvocationError, InvocationHandle,
    InvocationPollUsage, InvocationStep, LifecycleError, LoadTarget, ModuleDomainHandle,
    ModuleDomainRelease, Retain, RootHandle, Runtime, TableHandle, ValueHandle,
};

#[cfg(feature = "native")]
mod blocking;
#[cfg(feature = "native")]
mod invocation;
#[cfg(feature = "native")]
mod session;

#[cfg(feature = "native")]
pub use blocking::{BlockingRuntime, BlockingRuntimeError};
#[cfg(feature = "native")]
pub use invocation::{
    INVOCATION_WORKER_THREADS, InvocationAdmission, InvocationAdmissionError,
    InvocationCancellation, InvocationClass, InvocationCompletion, InvocationDiscardReason,
    InvocationLane, InvocationOwner, InvocationService, InvocationTask, InvocationTicket,
    InvocationTicketId, MAX_LOGICAL_LANES, MAX_PENDING_PER_LANE,
};
#[cfg(feature = "native")]
pub use session::{SharedRuntime, SharedRuntimeError, SharedRuntimeOutcome};

#[cfg(feature = "eval")]
mod eval;
#[cfg(feature = "eval")]
pub use eval::{
    DEFAULT_GAS, DEFAULT_MAX_MEMORY_BYTES, DEFAULT_TIMEOUT, Error, ErrorKind, Evaluator, Options,
    Output, StructuredErrorKind, Timing,
};
