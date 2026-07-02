use std::time::Duration;

use ruau_bytecode::CompileError;
use ruau_typecheck::diagnostics::Diagnostics;
use ruau_vm::{ExecutionFeatures, LoadError, RuntimeCapabilities};

use super::render;

/// Opaque tenant key used for request and admission accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TenantId(pub u64);

/// A fully-specified runner request.
#[derive(Clone, Debug)]
pub struct Request<'a> {
    /// Tenant attribution for ingress and aggregate accounting.
    pub tenant: TenantId,
    /// Optional per-request surface override.
    pub surface: Option<&'a ruau_surface::Surface>,
    /// Raw source bytes to parse, check, compile, and run.
    pub source: &'a [u8],
    /// Per-request deadline, cancellation, gas, and memory budget.
    pub budget: super::Budget,
}

impl<'a> Request<'a> {
    /// Builds a default-tenant request against the runner's configured surface.
    #[must_use]
    pub fn new(source: &'a [u8], budget: super::Budget) -> Self {
        Self {
            tenant: TenantId(0),
            surface: None,
            source,
            budget,
        }
    }

    /// Attributes the request to `tenant`.
    #[must_use]
    pub fn tenant(mut self, tenant: TenantId) -> Self {
        self.tenant = tenant;
        self
    }

    /// Overrides the runner surface for this request.
    #[must_use]
    pub fn surface(mut self, surface: &'a ruau_surface::Surface) -> Self {
        self.surface = Some(surface);
        self
    }
}

const DEFAULT_MAX_TYPE_DIAGNOSTICS: usize = 256;
const DEFAULT_MAX_PARSE_AST_NODES: usize = 500_000;
const DEFAULT_MAX_TYPE_ARENA_NODES: usize = 500_000;
const DEFAULT_MAX_COMPILED_INSTRUCTIONS: usize = 500_000;
const DEFAULT_MAX_COMPILED_BYTECODE_BYTES: usize = 4 * 1024 * 1024;

/// Limits enforced before a VM is built.
///
/// Source bytes use `Builder::max_source_bytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrontDoorLimits {
    /// Maximum parsed AST nodes allowed before checking or compiling.
    pub max_parse_ast_nodes: usize,
    /// Maximum type-check diagnostics returned in the error payload.
    pub max_type_diagnostics: usize,
    /// Maximum checker arena nodes after type-checking.
    pub max_type_arena_nodes: usize,
    /// Maximum instruction words in the compiled bytecode graph.
    pub max_compiled_instructions: usize,
    /// Maximum encoded bytecode bytes produced by compilation before VM loading.
    pub max_compiled_bytecode_bytes: usize,
}

impl Default for FrontDoorLimits {
    fn default() -> Self {
        Self {
            max_parse_ast_nodes: DEFAULT_MAX_PARSE_AST_NODES,
            max_type_diagnostics: DEFAULT_MAX_TYPE_DIAGNOSTICS,
            max_type_arena_nodes: DEFAULT_MAX_TYPE_ARENA_NODES,
            max_compiled_instructions: DEFAULT_MAX_COMPILED_INSTRUCTIONS,
            max_compiled_bytecode_bytes: DEFAULT_MAX_COMPILED_BYTECODE_BYTES,
        }
    }
}

/// Request caps checked before parser/checker/compiler work starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngressLimits {
    /// Maximum admitted requests across all tenants.
    pub max_in_flight: usize,
    /// Maximum admitted requests for one tenant.
    pub max_in_flight_per_tenant: usize,
}

impl IngressLimits {
    /// Conservative defaults derived from the lane count.
    #[must_use]
    pub fn fail_closed(lane_count: usize) -> Self {
        Self {
            max_in_flight: lane_count.saturating_mul(8).max(8),
            max_in_flight_per_tenant: lane_count.saturating_mul(2).max(2),
        }
    }

    /// No ingress caps. Spell this deliberately; there is no fail-open
    /// `Default`.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_in_flight: usize::MAX,
            max_in_flight_per_tenant: usize::MAX,
        }
    }
}

/// Per-tenant resource caps enforced across requests.
///
/// `None` leaves a resource uncapped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AggregateResourceLimits {
    /// Maximum requests recorded for one tenant.
    pub max_requests: Option<u64>,
    /// Maximum submitted source bytes recorded for one tenant.
    pub max_source_bytes: Option<u64>,
    /// Maximum parse/check/compile wall time recorded for one tenant.
    pub max_front_door_time: Option<Duration>,
    /// Maximum VM run wall time recorded for one tenant.
    pub max_run_time: Option<Duration>,
    /// Maximum VM gas recorded for one tenant.
    pub max_gas_spent: Option<u64>,
    /// Maximum charged bytes recorded for one tenant.
    pub max_charged_bytes: Option<u64>,
}

/// Aggregate resource dimension that stopped a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggregateResourceLimit {
    /// Total recorded requests.
    Requests,
    /// Total recorded source bytes.
    SourceBytes,
    /// Total recorded parse/check/compile wall-clock nanoseconds.
    FrontDoorTime,
    /// Total recorded VM run wall-clock nanoseconds.
    RunTime,
    /// Total recorded VM gas.
    GasSpent,
    /// Total recorded charged bytes.
    ChargedBytes,
}

/// Per-tenant resource totals accumulated from request reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TenantResourceTotals {
    /// Requests whose report contained parser/checker/compiler/load/run work.
    pub requests: u64,
    /// Submitted source bytes.
    pub source_bytes: u64,
    /// Parser, checker, and compiler wall time.
    pub front_door_time: Duration,
    /// VM run wall time.
    pub run_time: Duration,
    /// Parsed AST node total.
    pub parse_ast_nodes: u64,
    /// Type arena node total.
    pub type_arena_nodes: u64,
    /// Compiled bytecode instruction-word total.
    pub compiled_instructions: u64,
    /// Encoded bytecode byte total.
    pub compiled_bytecode_bytes: u64,
    /// VM gas total.
    pub gas_spent: u64,
    /// Host-initiated VM invocation total.
    pub vm_execution_count: u64,
    /// Last observed in-VM heap bytes.
    pub heap_bytes: u64,
    /// Peak observed in-VM heap bytes across this tenant's requests.
    pub peak_heap_bytes: u64,
    /// Charged byte total: source bytes plus encoded bytecode bytes plus peak
    /// in-VM heap bytes for each recorded request.
    pub charged_bytes: u64,
}

/// Pre-VM stage that exceeded a configured product limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontDoorStage {
    /// Parser product-size check.
    Parse,
    /// Type-checker product-size check.
    TypeCheck,
    /// Compiler product-size check.
    Compile,
}

/// Pre-VM product limit that was exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontDoorLimit {
    /// Parsed AST node count.
    ParseAstNodes,
    /// Checker arena node count.
    ArenaNodes,
    /// Compiled bytecode instruction count.
    CompiledInstructions,
    /// Encoded bytecode byte count.
    CompiledBytecodeBytes,
}

/// Which ingress cap rejected a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressScope {
    /// Pool-wide in-flight cap.
    Pool,
    /// Per-tenant in-flight cap.
    Tenant,
}

/// A per-request budget could not be constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetError {
    /// The requested deadline has already elapsed.
    DeadlineElapsed,
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::DeadlineElapsed => "request deadline has already elapsed",
        };
        write!(f, "request budget rejected: {reason}")
    }
}

impl std::error::Error for BudgetError {}

/// One returned value, rendered into owned data before the VM is dropped.
/// Strings and buffers are copied; tables are snapshotted as owned key/value
/// pairs; values not yet supported by the marshaler are recorded by Luau type.
#[derive(Clone, Debug, PartialEq)]
pub enum ResultValue {
    /// `nil`.
    Nil,
    /// A boolean.
    Boolean(bool),
    /// A floating-point number.
    Number(f64),
    /// An integer.
    Integer(i64),
    /// A 3-lane vector.
    Vector([f32; 3]),
    /// A string's bytes, copied out of the heap.
    String(Vec<u8>),
    /// A buffer's bytes, copied out of the heap.
    Buffer(Vec<u8>),
    /// A table snapshot in raw Luau iteration order.
    Table(Vec<(Self, Self)>),
    /// A heap value that cannot escape the VM, named by its Luau type
    /// (`"function"`, `"userdata"`, or `"thread"`).
    Opaque(&'static str),
}

/// Why a request failed.
#[derive(Clone, Debug)]
pub enum RequestError {
    /// Source exceeded the configured byte cap.
    SourceTooLarge {
        /// The submitted source length.
        bytes: usize,
        /// The configured cap.
        cap: usize,
    },
    /// Type-checking failed. Diagnostics are capped by [`FrontDoorLimits`].
    TypeErrors(Diagnostics),
    /// The source failed to compile.
    Compile(CompileError),
    /// A pre-VM product exceeded its configured limit.
    FrontDoorLimitExceeded {
        /// Pipeline stage that exceeded the limit.
        stage: FrontDoorStage,
        /// Limit that was exceeded.
        limit: FrontDoorLimit,
        /// Observed count.
        used: usize,
        /// Configured cap.
        cap: usize,
    },
    /// Ingress admission rejected the request.
    IngressRejected {
        /// Tenant whose request was rejected.
        tenant: TenantId,
        /// Current in-flight count for the rejected scope.
        in_flight: usize,
        /// Configured cap that was reached.
        cap: usize,
        /// Whether the cap was pool-wide or per-tenant.
        scope: IngressScope,
    },
    /// An aggregate resource budget rejected the request.
    AggregateResourceLimitExceeded {
        /// Tenant whose aggregate budget was exhausted.
        tenant: TenantId,
        /// Aggregate resource that exceeded its cap.
        limit: AggregateResourceLimit,
        /// Recorded or projected usage.
        used: u128,
        /// Configured cap.
        cap: u128,
    },
    /// The lane pool rejected the request before VM execution.
    LaneAdmissionRejected {
        /// Tenant whose lane submission was rejected.
        tenant: TenantId,
    },
    /// Validated load rejected the bytecode.
    Load(LoadError),
    /// Sandbox installation failed (out of memory under the configured cap).
    SandboxFailed,
    /// The request was cancelled — the caller tripped the cancellation token
    /// before the wall-clock deadline.
    Cancelled,
    /// The request exceeded its wall-clock deadline.
    DeadlineExceeded,
    /// The request hit the memory cap; the rendered error value is attached.
    OutOfMemory(ResultValue),
    /// The VM refused work after a contained panic poisoned it, or this request
    /// caught the panic and poisoned the VM.
    PanicPoison(ResultValue),
    /// An `xpcall` message handler failed while replacing an error.
    HandlerFailure(ResultValue),
    /// The script raised an ordinary uncaught runtime error, rendered to owned
    /// data.
    Runtime(ResultValue),
}

/// Stable failure category used by request reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureCategory {
    /// Source exceeded the configured byte cap.
    SourceTooLarge,
    /// Type-checking failed.
    TypeErrors,
    /// Compilation failed.
    Compile,
    /// A pre-VM product limit failed.
    FrontDoorLimit,
    /// Ingress admission rejected the request.
    IngressRejected,
    /// A tenant aggregate resource budget rejected the request.
    AggregateResourceLimit,
    /// Lane-pool admission rejected the request before VM execution.
    LaneAdmissionRejected,
    /// Validated bytecode load failed.
    Load,
    /// Sandbox installation failed.
    SandboxFailed,
    /// Caller cancellation stopped the request.
    Cancelled,
    /// The request deadline stopped the request.
    DeadlineExceeded,
    /// The VM hit its memory cap.
    OutOfMemory,
    /// The request caught a host panic and poisoned the VM.
    PanicPoison,
    /// An `xpcall` message handler failed.
    HandlerFailure,
    /// Ordinary uncaught runtime error.
    Runtime,
}

/// External stop reason for a failed request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// The caller's cancellation token fired.
    Cancelled,
    /// The wall-clock deadline elapsed.
    Deadline,
}

impl RequestError {
    /// Stable failure category for report envelopes.
    #[must_use]
    pub const fn category(&self) -> FailureCategory {
        match self {
            Self::SourceTooLarge { .. } => FailureCategory::SourceTooLarge,
            Self::TypeErrors(_) => FailureCategory::TypeErrors,
            Self::Compile(_) => FailureCategory::Compile,
            Self::FrontDoorLimitExceeded { .. } => FailureCategory::FrontDoorLimit,
            Self::IngressRejected { .. } => FailureCategory::IngressRejected,
            Self::AggregateResourceLimitExceeded { .. } => FailureCategory::AggregateResourceLimit,
            Self::LaneAdmissionRejected { .. } => FailureCategory::LaneAdmissionRejected,
            Self::Load(_) => FailureCategory::Load,
            Self::SandboxFailed => FailureCategory::SandboxFailed,
            Self::Cancelled => FailureCategory::Cancelled,
            Self::DeadlineExceeded => FailureCategory::DeadlineExceeded,
            Self::OutOfMemory(_) => FailureCategory::OutOfMemory,
            Self::PanicPoison(_) => FailureCategory::PanicPoison,
            Self::HandlerFailure(_) => FailureCategory::HandlerFailure,
            Self::Runtime(_) => FailureCategory::Runtime,
        }
    }

    /// Deadline/cancellation reason when this error came from an external stop.
    #[must_use]
    pub const fn stop_reason(&self) -> Option<StopReason> {
        match self {
            Self::Cancelled => Some(StopReason::Cancelled),
            Self::DeadlineExceeded => Some(StopReason::Deadline),
            _ => None,
        }
    }
}

impl FrontDoorStage {
    const fn label(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::TypeCheck => "type-check",
            Self::Compile => "compile",
        }
    }
}

impl FrontDoorLimit {
    const fn label(self) -> &'static str {
        match self {
            Self::ParseAstNodes => "parsed AST nodes",
            Self::ArenaNodes => "checker arena nodes",
            Self::CompiledInstructions => "compiled instructions",
            Self::CompiledBytecodeBytes => "encoded bytecode bytes",
        }
    }
}

impl IngressScope {
    const fn label(self) -> &'static str {
        match self {
            Self::Pool => "pool-wide",
            Self::Tenant => "per-tenant",
        }
    }
}

impl AggregateResourceLimit {
    /// Returns a short human-readable name for this aggregate resource dimension.
    const fn label(self) -> &'static str {
        match self {
            Self::Requests => "request",
            Self::SourceBytes => "source byte",
            Self::FrontDoorTime => "parse/check/compile nanosecond",
            Self::RunTime => "run nanosecond",
            Self::GasSpent => "gas",
            Self::ChargedBytes => "charged byte",
        }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceTooLarge { bytes, cap } => {
                write!(
                    f,
                    "request rejected: source is {bytes} bytes, over the {cap}-byte cap"
                )
            }
            Self::TypeErrors(diagnostics) => write!(
                f,
                "request rejected: type-checking found {} error(s)",
                diagnostics.len()
            ),
            Self::Compile(err) => write!(f, "request rejected: compilation failed: {err}"),
            Self::FrontDoorLimitExceeded {
                stage,
                limit,
                used,
                cap,
            } => write!(
                f,
                "request rejected: {} stage exceeded its {} limit ({used} over cap {cap})",
                stage.label(),
                limit.label()
            ),
            Self::IngressRejected {
                tenant,
                in_flight,
                cap,
                scope,
            } => write!(
                f,
                "request rejected: tenant {} ingress {} in-flight count {in_flight} reached cap {cap}",
                tenant.0,
                scope.label()
            ),
            Self::AggregateResourceLimitExceeded {
                tenant,
                limit,
                used,
                cap,
            } => write!(
                f,
                "request rejected: tenant {} aggregate {} usage {used} reached cap {cap}",
                tenant.0,
                limit.label()
            ),
            Self::LaneAdmissionRejected { tenant } => write!(
                f,
                "request rejected: tenant {} lane-pool admission rejected the run before execution",
                tenant.0
            ),
            Self::Load(err) => write!(f, "request rejected: bytecode load failed: {err}"),
            Self::SandboxFailed => {
                f.write_str("request failed: sandbox installation hit the memory cap")
            }
            Self::Cancelled => f.write_str("request cancelled before its deadline"),
            Self::DeadlineExceeded => f.write_str("request exceeded its wall-clock deadline"),
            Self::OutOfMemory(value) => {
                write!(
                    f,
                    "request hit the memory cap: {}",
                    render::render_error_value(value)
                )
            }
            Self::PanicPoison(value) => write!(
                f,
                "request poisoned the VM after a host panic: {}",
                render::render_error_value(value)
            ),
            Self::HandlerFailure(value) => write!(
                f,
                "xpcall message handler failed: {}",
                render::render_error_value(value)
            ),
            Self::Runtime(value) => write!(
                f,
                "script raised an uncaught error: {}",
                render::render_error_value(value)
            ),
        }
    }
}

impl std::error::Error for RequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Compile(err) => Some(err),
            Self::Load(err) => Some(err),
            _ => None,
        }
    }
}

/// Per-request resource and timing metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RequestMetrics {
    /// Tenant source size in bytes.
    pub source_bytes: usize,
    /// Time spent in the parser product-size guard.
    pub parse_time: Duration,
    /// Time spent type-checking.
    pub check_time: Duration,
    /// Time spent compiling.
    pub compile_time: Duration,
    /// Time spent building the per-request VM and installing its capability surface.
    pub vm_build_time: Duration,
    /// Time spent installing the per-request sandbox.
    pub sandbox_time: Duration,
    /// Time spent loading bytecode.
    pub load_time: Duration,
    /// Time spent executing.
    pub run_time: Duration,
    /// Parsed AST node count.
    pub parse_ast_nodes: usize,
    /// Type-checker arena nodes.
    pub type_arena_nodes: usize,
    /// Compiled bytecode instruction words.
    pub compiled_instructions: usize,
    /// Encoded bytecode bytes produced by compilation.
    pub compiled_bytecode_bytes: usize,
    /// Gas units spent by VM execution.
    pub gas_spent: u64,
    /// Host-initiated invocations observed on the per-request VM.
    pub vm_execution_count: u64,
    /// In-VM heap bytes in use when the request finished.
    pub heap_bytes: usize,
    /// Highest in-VM heap byte total observed during the request.
    pub peak_heap_bytes: usize,
    /// Completed GC cycles during the request.
    pub gc_cycles: u64,
}

/// What a successful request produced.
#[derive(Clone, Debug)]
pub struct RequestOutcome {
    /// The script's return values, rendered to owned data.
    pub values: Vec<ResultValue>,
    /// Resource and timing metrics for the request.
    pub metrics: RequestMetrics,
}

/// Static request metadata emitted with every report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestReportMetadata {
    /// Selected VM runtime capabilities.
    pub runtime_capabilities: RuntimeCapabilities,
    /// Explicit execution feature switches.
    pub features: ExecutionFeatures,
    /// Whether this request surface grants runtime `require` through a source.
    pub module_source_granted: bool,
    /// VM crate version.
    pub vm_version: &'static str,
    /// Revision hash of the committed conformance scope manifest.
    pub conformance_revision: u64,
    /// Revision hash of the configured host-module declaration manifest.
    pub host_module_manifest_version: u64,
}

/// Request result carried by a report envelope.
#[derive(Clone, Debug)]
pub enum RequestReportOutcome {
    /// Successful request return values.
    Success {
        /// The script's return values, rendered to owned data.
        values: Vec<ResultValue>,
    },
    /// Typed failure.
    Failure {
        /// The request error.
        error: RequestError,
    },
}

/// Report envelope for a request run.
#[derive(Clone, Debug)]
pub struct RequestReport {
    /// Tenant this request was attributed to.
    pub tenant: TenantId,
    /// Success or typed failure.
    pub outcome: RequestReportOutcome,
    /// Timing and resource metrics, populated as far as the pipeline reached.
    pub metrics: RequestMetrics,
    /// Static runner/build metadata for this request.
    pub metadata: RequestReportMetadata,
    /// Stable failure category, if the request failed.
    pub failure_category: Option<FailureCategory>,
    /// Deadline or cancellation reason, if one stopped the request.
    pub stop_reason: Option<StopReason>,
}

impl RequestReport {
    /// Converts the report into the convenience success-or-error shape.
    pub fn into_result(self) -> Result<RequestOutcome, RequestError> {
        match self.outcome {
            RequestReportOutcome::Success { values } => Ok(RequestOutcome {
                values,
                metrics: self.metrics,
            }),
            RequestReportOutcome::Failure { error } => Err(error),
        }
    }
}
