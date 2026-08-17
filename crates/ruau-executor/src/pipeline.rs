use std::{
    sync::{Arc, Mutex, OnceLock, atomic::AtomicBool},
    time::{Duration, Instant},
};

use ruau_bytecode::{BytecodeChunk, CompileErrorKind, CompileOptions, encode_chunk};
use ruau_source::{ModuleId, RootSource, SourceProvider};
use ruau_surface::{Surface, VmConfig};
#[cfg(any())]
use ruau_syntax::parse::parse_module_bytes_with_config;
use ruau_syntax::parse::{Config, ParsedModule, parse_shared_module_bytes_with_config};
use ruau_typecheck::{Config as CheckConfig, Diagnostics, GraphChecker, config::EmptyResolver};
use ruau_vm::{
    Ambient, CallOptions, Cancel, CancellationFlag, Deadline, ExecError, ExecutionFeatures, Limits,
    LoadError, RuntimeCompileContext, RuntimeCompiler, RuntimeErrorKind, StopReason, Vm,
};

use super::{
    TenantId,
    admission::{
        AccountedResources, IngressAdmission, IngressGuard, TenantResourceAccounting,
        TenantResourceReservation,
    },
    builder::Builder,
    preflight::{PreflightCache, PreflightOutcome, PreflightVerdict},
    render::{request_report_error, request_report_success},
    run_control::RunControl,
    types::{
        AggregateResourceLimits, PreflightLimit, PreflightLimits, PreflightStage, RequestError,
        RequestMetrics, RunMetadata, RunOutcome, RunReport, TenantResourceTotals,
    },
};
use crate::lanes::{LaneMetrics, LanePool};

const DEFAULT_REQUEST_QUANTUM: u64 = 4_096;
const RUNTIME_COMPILE_MODULE_ID: &str = "__executor_runtime_compile__";
static EMPTY_CONFIG_RESOLVER: EmptyResolver = EmptyResolver;
const RUNNER_REQUEST_MODULE_ID: &str = "__executor_request__";
static FRONT_DOOR_ASYNC_RUNTIME: OnceLock<Result<Arc<PreflightAsyncRuntimePool>, String>> =
    OnceLock::new();

fn update_cache_key_field(hasher: &mut blake3::Hasher, field: &[u8]) {
    hasher.update(&field.len().to_le_bytes());
    hasher.update(field);
}

pub fn compile_policy_fingerprint(compile_policy: &CompileOptions) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    update_cache_key_field(&mut hasher, &[compile_policy.optimization_level]);
    update_cache_key_field(&mut hasher, &[compile_policy.debug_level]);
    update_cache_key_field(&mut hasher, &[compile_policy.coverage_level]);
    *hasher.finalize().as_bytes()
}

pub fn preflight_cache_key(
    surface: &Surface,
    source: &[u8],
    compile_policy_fingerprint: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    update_cache_key_field(&mut hasher, &surface.cache_fingerprint());
    update_cache_key_field(&mut hasher, source);
    update_cache_key_field(&mut hasher, compile_policy_fingerprint);
    if let Some((identity, epoch)) = surface.module_source_cache_stamp() {
        update_cache_key_field(&mut hasher, &[1]);
        update_cache_key_field(&mut hasher, &identity.to_le_bytes());
        update_cache_key_field(&mut hasher, &epoch.to_le_bytes());
    } else {
        update_cache_key_field(&mut hasher, &[0]);
        update_cache_key_field(&mut hasher, &0_u64.to_le_bytes());
        update_cache_key_field(&mut hasher, &0_u64.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

#[derive(Debug)]
struct SourcePreflightCheck {
    has_issues: bool,
    diagnostics: Diagnostics,
    type_arena_nodes: usize,
}

#[derive(Clone)]
struct PreflightEngine {
    surface: Arc<Surface>,
    compile_policy: CompileOptions,
    limits: PreflightLimits,
}

#[derive(Clone, Debug)]
enum ReadyPreflightSourceIdentity {
    Runtime(Option<ModuleId>),
    Sourceless,
}

#[derive(Debug)]
struct CheckedPreflight {
    parsed: ParsedModule,
    diagnostics: Diagnostics,
    type_arena_nodes: usize,
}

#[derive(Debug)]
enum PreflightCheckOutcome {
    Accepted(CheckedPreflight),
    TypeErrors {
        diagnostics: Diagnostics,
        ast_nodes: usize,
        type_arena_nodes: usize,
    },
}

#[derive(Debug)]
struct EnginePreflightOutcome {
    diagnostics: Diagnostics,
    ast_nodes: usize,
    type_arena_nodes: usize,
    bytecode: CompiledBytecodeMetrics,
    chunk: BytecodeChunk,
}

#[derive(Debug)]
enum PreflightEngineError {
    Limit {
        stage: PreflightStage,
        limit: PreflightLimit,
        used: usize,
        cap: usize,
    },
    TypeErrors(Diagnostics),
    Compile(ruau_bytecode::CompileError),
    Product(RequestError),
    Cancelled,
}

impl PreflightEngineError {
    fn into_request_error(self) -> RequestError {
        match self {
            Self::Limit {
                stage,
                limit,
                used,
                cap,
            } => RequestError::PreflightLimitExceeded {
                stage,
                limit,
                used,
                cap,
            },
            Self::TypeErrors(diagnostics) => RequestError::TypeErrors(diagnostics),
            Self::Compile(error) => RequestError::Compile(error),
            Self::Product(error) => error,
            Self::Cancelled => RequestError::Cancelled,
        }
    }

    fn into_runtime_message(self) -> Vec<u8> {
        match self {
            Self::Limit {
                limit, used, cap, ..
            } => {
                let label = match limit {
                    PreflightLimit::ParseAstNodes => "parse AST node",
                    PreflightLimit::ArenaNodes => "type arena node",
                    PreflightLimit::CompiledInstructions => "compiled instruction",
                    PreflightLimit::CompiledBytecodeBytes => "compiled bytecode byte",
                };
                format!("runtime compilation {label} limit exceeded: {used} > {cap}").into_bytes()
            }
            Self::TypeErrors(diagnostics) => {
                format!("runtime compilation type check failed: {diagnostics:?}").into_bytes()
            }
            Self::Compile(error) if error.kind() == CompileErrorKind::Cancelled => {
                runtime_compilation_cancelled()
            }
            Self::Compile(error) => error.to_string().into_bytes(),
            Self::Product(error) => {
                format!("runtime compilation product failed: {error:?}").into_bytes()
            }
            Self::Cancelled => runtime_compilation_cancelled(),
        }
    }
}

impl PreflightEngine {
    fn parse(
        &self,
        source: Arc<[u8]>,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<ParsedModule, PreflightEngineError> {
        self.check_cancelled(cancel)?;
        let mut config = Config::upstream_default();
        config.capture_comments = true;
        let parsed = parse_shared_module_bytes_with_config(source, &config);
        self.check_cancelled(cancel)?;
        self.enforce_limit(
            PreflightStage::Parse,
            PreflightLimit::ParseAstNodes,
            parsed.ast_nodes(),
            self.limits.max_parse_ast_nodes,
        )?;
        Ok(parsed)
    }

    async fn check_async(
        &self,
        parsed: ParsedModule,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<PreflightCheckOutcome, PreflightEngineError> {
        let check = if self.surface.module_source().is_none()
            && std::str::from_utf8(parsed.source()).is_err()
        {
            check_sourceless_parsed_module(
                &self.surface,
                &parsed,
                self.limits.max_type_diagnostics,
                cancel,
            )
        } else {
            check_root_source_async(
                &self.surface,
                parsed.clone(),
                self.surface.module_source(),
                self.limits.max_type_diagnostics,
                cancel,
            )
            .await
        };
        self.classify_check(parsed, check)
    }

    fn check_ready(
        &self,
        parsed: ParsedModule,
        identity: ReadyPreflightSourceIdentity,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<PreflightCheckOutcome, PreflightEngineError> {
        let check = match identity {
            ReadyPreflightSourceIdentity::Runtime(module_id)
                if module_id.is_some() || std::str::from_utf8(parsed.source()).is_ok() =>
            {
                check_runtime_source_ready(
                    &self.surface,
                    &parsed,
                    module_id,
                    self.limits.max_type_diagnostics,
                    cancel,
                )
            }
            ReadyPreflightSourceIdentity::Runtime(_) | ReadyPreflightSourceIdentity::Sourceless => {
                check_sourceless_parsed_module(
                    &self.surface,
                    &parsed,
                    self.limits.max_type_diagnostics,
                    cancel,
                )
            }
        };
        self.classify_check(parsed, check)
    }

    fn classify_check(
        &self,
        parsed: ParsedModule,
        check: SourcePreflightCheck,
    ) -> Result<PreflightCheckOutcome, PreflightEngineError> {
        self.enforce_limit(
            PreflightStage::TypeCheck,
            PreflightLimit::ArenaNodes,
            check.type_arena_nodes,
            self.limits.max_type_arena_nodes,
        )?;
        if check.has_issues {
            return Ok(PreflightCheckOutcome::TypeErrors {
                diagnostics: check.diagnostics,
                ast_nodes: parsed.ast_nodes(),
                type_arena_nodes: check.type_arena_nodes,
            });
        }
        Ok(PreflightCheckOutcome::Accepted(CheckedPreflight {
            parsed,
            diagnostics: check.diagnostics,
            type_arena_nodes: check.type_arena_nodes,
        }))
    }

    fn compile(
        &self,
        checked: CheckedPreflight,
        cancel: Option<Arc<AtomicBool>>,
        compiled_caps: CompiledBytecodeMetrics,
    ) -> Result<EnginePreflightOutcome, PreflightEngineError> {
        self.check_cancelled(cancel.as_ref())?;
        let CheckedPreflight {
            parsed,
            diagnostics,
            type_arena_nodes,
        } = checked;
        let chunk = self
            .surface
            .runtime_capabilities()
            .compile_parsed_module_with_cancel(&parsed, &self.compile_policy, cancel)
            .map_err(PreflightEngineError::Compile)?;
        let bytecode = compiled_bytecode_metrics(&chunk).map_err(PreflightEngineError::Product)?;
        self.enforce_limit(
            PreflightStage::Compile,
            PreflightLimit::CompiledInstructions,
            bytecode.instructions,
            compiled_caps.instructions,
        )?;
        self.enforce_limit(
            PreflightStage::Compile,
            PreflightLimit::CompiledBytecodeBytes,
            bytecode.encoded_bytes,
            compiled_caps.encoded_bytes,
        )?;
        Ok(EnginePreflightOutcome {
            diagnostics,
            ast_nodes: parsed.ast_nodes(),
            type_arena_nodes,
            bytecode,
            chunk,
        })
    }

    fn run_ready(
        &self,
        source: Arc<[u8]>,
        identity: ReadyPreflightSourceIdentity,
        cancel: Option<&Arc<AtomicBool>>,
        compiled_caps: CompiledBytecodeMetrics,
    ) -> Result<EnginePreflightOutcome, PreflightEngineError> {
        self.check_cancelled(cancel)?;
        let parsed = self.parse(source, cancel)?;
        self.check_cancelled(cancel)?;
        let checked = match self.check_ready(parsed, identity, cancel.cloned())? {
            PreflightCheckOutcome::Accepted(checked) => checked,
            PreflightCheckOutcome::TypeErrors { diagnostics, .. } => {
                return Err(PreflightEngineError::TypeErrors(diagnostics));
            }
        };
        self.check_cancelled(cancel)?;
        let outcome = self.compile(checked, cancel.cloned(), compiled_caps)?;
        self.check_cancelled(cancel)?;
        Ok(outcome)
    }

    fn check_cancelled(
        &self,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<(), PreflightEngineError> {
        if cancel.is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err(PreflightEngineError::Cancelled);
        }
        Ok(())
    }

    fn default_compiled_caps(&self) -> CompiledBytecodeMetrics {
        CompiledBytecodeMetrics {
            instructions: self.limits.max_compiled_instructions,
            encoded_bytes: self.limits.max_compiled_bytecode_bytes,
        }
    }

    fn enforce_limit(
        &self,
        stage: PreflightStage,
        limit: PreflightLimit,
        used: usize,
        cap: usize,
    ) -> Result<(), PreflightEngineError> {
        if used > cap {
            return Err(PreflightEngineError::Limit {
                stage,
                limit,
                used,
                cap,
            });
        }
        Ok(())
    }
}

fn checked_frontend_for_root<'source>(
    surface: &Surface,
    source: &'source RootSource,
    parse_config: Config,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> GraphChecker<'source> {
    let mut checker = surface.new_checker();
    if let Some(cancel) = cancel {
        checker.set_cancel_flag(cancel);
    }
    let mut frontend = GraphChecker::with_checker(source, &EMPTY_CONFIG_RESOLVER, checker);
    frontend.set_parse_config(parse_config);
    frontend.set_source_mode_override(Some(surface.analysis_mode()));
    frontend
}

fn source_preflight_check_from_frontend(
    frontend: &GraphChecker<'_>,
    graph: &ruau_typecheck::CheckedGraph,
    max_type_diagnostics: usize,
) -> SourcePreflightCheck {
    let diagnostics = graph.diagnostics().clone().into_flat_diagnostics();
    let has_issues = diagnostics.has_issues();
    let diagnostics = diagnostics.capped(max_type_diagnostics);
    SourcePreflightCheck {
        has_issues,
        diagnostics,
        type_arena_nodes: frontend.checker().arena().type_len()
            + frontend.checker().arena().pack_len(),
    }
}

fn check_sourceless_parsed_module(
    surface: &Surface,
    parsed: &ParsedModule,
    max_type_diagnostics: usize,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> SourcePreflightCheck {
    let mut checker = surface.new_checker();
    if let Some(cancel) = cancel {
        checker.set_cancel_flag(cancel);
    }
    let mut config = CheckConfig::with_source_mode(surface.analysis_mode());
    config.parse = parsed.config();
    let checked = checker.check_parsed_module_with_config(parsed, config);
    let has_issues = checked.has_issues();
    let diagnostics = checked.diagnostics().clone().capped(max_type_diagnostics);
    SourcePreflightCheck {
        has_issues,
        diagnostics,
        type_arena_nodes: checker.arena().type_len() + checker.arena().pack_len(),
    }
}

async fn check_root_source_async(
    surface: &Surface,
    parsed: ParsedModule,
    module_source: Option<Arc<dyn SourceProvider>>,
    max_type_diagnostics: usize,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> SourcePreflightCheck {
    let mut source = RootSource::new(
        ModuleId::canonicalized(RUNNER_REQUEST_MODULE_ID),
        parsed.source().to_vec(),
    )
    .with_display_name("request");
    if let Some(module_source) = module_source {
        source = source.with_delegate(module_source);
    }
    let root = source.root_name();
    let mut frontend = checked_frontend_for_root(surface, &source, parsed.config(), cancel);
    let graph = frontend
        .check_parsed_graph(root, parsed)
        .await
        .expect("executor graph checker is unlimited");
    source_preflight_check_from_frontend(&frontend, &graph, max_type_diagnostics)
}

fn check_runtime_source_ready(
    surface: &Surface,
    parsed: &ParsedModule,
    module_id: Option<ModuleId>,
    max_type_diagnostics: usize,
    cancel: Option<Arc<AtomicBool>>,
) -> SourcePreflightCheck {
    let module_source = surface.module_source();
    let root_id = module_id
        .as_ref()
        .and_then(|id| id.as_str().map(ModuleId::canonicalized))
        .unwrap_or_else(|| ModuleId::canonicalized(RUNTIME_COMPILE_MODULE_ID));
    let mut source =
        RootSource::new(root_id, parsed.source().to_vec()).with_display_name("runtime compilation");
    if let Some(module_id) = module_id {
        source = source.with_root_requester(module_id);
    }
    if let Some(module_source) = module_source {
        source = source.with_delegate(module_source);
    }
    let root = source.root_name();
    let mut frontend = checked_frontend_for_root(surface, &source, parsed.config(), cancel);
    let graph = frontend
        .check_parsed_graph_blocking(root, parsed.clone())
        .expect("executor graph checker is unlimited");
    source_preflight_check_from_frontend(&frontend, &graph, max_type_diagnostics)
}

#[cfg(any())]
pub fn runtime_source_check_cancelled_for_test(surface: &Surface, source: &[u8]) -> bool {
    let cancel = Arc::new(AtomicBool::new(true));
    let parsed = parse_module_bytes_with_config(source, &Config::upstream_default());
    check_runtime_source_ready(surface, &parsed, None, usize::MAX, Some(cancel)).has_issues
}

/// Bounded request executor built from shared configuration.
///
/// Per-request source and budget are passed to [`Executor::run`].
pub struct Executor {
    pub(super) surface: Arc<Surface>,
    pub(super) ambient: Ambient,
    pub(super) base_limits: Limits,
    pub(super) features: ExecutionFeatures,
    pub(super) max_source_bytes: usize,
    pub(super) compile_policy: CompileOptions,
    pub(super) compile_policy_fingerprint: [u8; 32],
    #[cfg(any())]
    pub(crate) preflight: PreflightLimits,
    #[allow(clippy::cfg_not_test)] // production visibility; tests use the `pub(crate)` field above
    #[cfg(not(any()))]
    pub(super) preflight: PreflightLimits,
    pub(super) ingress: Arc<IngressAdmission>,
    pub(super) aggregate_limits: AggregateResourceLimits,
    pub(super) resource_accounting: Arc<TenantResourceAccounting>,
    pub(super) lane_pool: LanePool,
    /// Bounded admission for CPU-heavy parser/checker/compiler work.
    pub(super) preflight_permits: Arc<tokio::sync::Semaphore>,
    pub(super) preflight_cache: PreflightCache,
    #[cfg(any())]
    pub(crate) runtime_compiler_override: Option<Arc<dyn RuntimeCompiler>>,
}

impl Executor {
    /// Starts a fail-closed builder.
    #[must_use]
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// The selected VM runtime capabilities.
    #[must_use]
    pub fn runtime_capabilities(&self) -> &ruau_vm::RuntimeCapabilities {
        self.surface.runtime_capabilities()
    }

    /// The exact request capability surface.
    #[must_use]
    pub fn surface(&self) -> &Arc<Surface> {
        &self.surface
    }

    /// The execution feature set. Defaults to all off.
    #[must_use]
    pub fn features(&self) -> ExecutionFeatures {
        self.features
    }

    /// The configured source byte cap.
    #[must_use]
    pub fn max_source_bytes(&self) -> usize {
        self.max_source_bytes
    }

    /// Static metadata included with reports for the default surface.
    #[must_use]
    pub fn report_metadata(&self) -> RunMetadata {
        self.report_metadata_for_surface(&self.surface)
    }

    /// Static metadata for a request that uses `surface`.
    #[must_use]
    pub fn report_metadata_for_surface(&self, surface: &Surface) -> RunMetadata {
        RunMetadata {
            runtime_capabilities: surface.runtime_capabilities().clone(),
            features: self.features,
            module_source_granted: surface.has_module_source(),
            vm_version: ruau_vm::version(),
            conformance_revision: ruau_vm::conformance_scope_revision(),
            host_module_manifest_version: surface.host_module_manifest_version(),
        }
    }

    /// Current aggregate resource totals for `tenant`.
    #[must_use]
    pub fn tenant_resource_totals(&self, tenant: TenantId) -> TenantResourceTotals {
        self.resource_accounting.totals(tenant)
    }

    /// The number of worker lanes this executor dispatches VM work across.
    #[must_use]
    pub fn lane_count(&self) -> usize {
        self.lane_pool.lane_count()
    }

    /// Current lane-pool admission and lifecycle metrics.
    #[must_use]
    pub fn lane_metrics(&self) -> LaneMetrics {
        self.lane_pool.metrics()
    }

    /// Runs `request` and returns success values or a typed error.
    ///
    /// `request.source` is raw bytes; non-UTF-8 bytes inside string literals are
    /// preserved through parse, check, and compile.
    ///
    /// # Errors
    /// Returns a [`RequestError`] for rejection, cancellation, deadline, load,
    /// sandbox, or runtime failure.
    pub async fn run(&self, request: super::Request<'_>) -> Result<RunOutcome, RequestError> {
        self.run_report(request).await.into_result()
    }

    /// Runs `request` and always returns a report, including failures.
    pub async fn run_report(&self, request: super::Request<'_>) -> RunReport {
        let tenant = request.tenant();
        let surface = request
            .surface()
            .map_or_else(|| Arc::clone(&self.surface), Arc::clone);
        let source = request.source();
        let run_control = request.into_run_control().scoped();
        self.run_report_for_tenant_inner(tenant, surface, source, run_control)
            .await
    }

    async fn run_report_for_tenant_inner(
        &self,
        tenant: TenantId,
        surface: Arc<Surface>,
        source: &[u8],
        budget: RunControl,
    ) -> RunReport {
        let AdmittedRequest {
            metadata,
            mut metrics,
            ingress,
            reservation,
        } = match self.admit_request(tenant, &surface, source.len(), &budget) {
            Ok(admitted) => admitted,
            Err(report) => return *report,
        };

        let chunk = match self
            .run_preflight_pipeline(
                &surface,
                source,
                &budget,
                tenant,
                metadata.clone(),
                &mut metrics,
            )
            .await
        {
            Ok(chunk) => chunk,
            Err(report) => return self.finalize_admitted_report(*report, reservation),
        };

        // 6. Move VM-owned work onto the lane pool. The executor task keeps the
        //    source cap, ingress guard, preflight work, request metadata, and
        //    deadline timer. The lane closure owns VM build/sandbox/load/run,
        //    rendering, and VM metrics because those all borrow the VM.
        let exec_cancel = budget.cancel.child();
        let mut limits = self.base_limits.clone();
        limits.deadline = Some(Deadline::Wall(budget.deadline));
        limits.cancel = Some(exec_cancel.clone());
        if limits.quantum.is_none() {
            limits.quantum = Some(DEFAULT_REQUEST_QUANTUM);
        }
        // 7. The scoped run-control signal already bridges the request deadline
        //    to the VM safepoint. The wall deadline also gates parked awaits.
        let lane_surface = surface.clone();
        let lane_ambient = self.ambient;
        let lane_runtime_compiler = self.runtime_compiler_for_surface(&surface);
        let lane_cancel = exec_cancel.clone();
        let lane_metrics = metrics;
        let lane_accounting = LaneAccounting::new(reservation, ingress, metrics);
        let Some(submission) = self
            .lane_pool
            .submit_cancellable(tenant, move || async move {
                let request = LaneRequest {
                    surface: lane_surface,
                    ambient: lane_ambient,
                    limits,
                    runtime_compiler: lane_runtime_compiler,
                    chunk,
                    exec_cancel: lane_cancel,
                    metrics: lane_metrics,
                    accounting: lane_accounting,
                };
                run_request_vm_on_lane(request).await
            })
        else {
            return request_report_error(
                RequestError::LaneAdmissionRejected { tenant },
                metrics,
                metadata,
                tenant,
            );
        };

        let lane_deadline =
            tokio::time::sleep_until(tokio::time::Instant::from_std(budget.deadline));
        tokio::pin!(lane_deadline);
        let lane_result = tokio::select! {
            biased;
            () = budget.cancel.cancelled() => {
                let error = request_error_from_stop(
                    budget.cancel.stop_reason().unwrap_or(StopReason::Cancelled),
                );
                return request_report_error(error, metrics, metadata, tenant);
            }
            () = &mut lane_deadline => {
                budget.cancel.stop(StopReason::Deadline);
                return request_report_error(
                    RequestError::DeadlineExceeded,
                    metrics,
                    metadata,
                    tenant,
                );
            }
            result = submission.recv() => match result {
                Ok(result) => result,
                Err(_) => {
                    return request_report_error(
                        RequestError::Runtime(ruau_vm::ValueSnapshot::String(
                            b"lane pool dropped request".to_vec(),
                        )),
                        metrics,
                        metadata,
                        tenant,
                    );
                }
            },
        };

        match lane_result.outcome {
            Ok(values) => request_report_success(values, lane_result.metrics, metadata, tenant),
            Err(error) => request_report_error(error, lane_result.metrics, metadata, tenant),
        }
    }

    fn admit_request(
        &self,
        tenant: TenantId,
        surface: &Surface,
        source_bytes: usize,
        budget: &RunControl,
    ) -> Result<AdmittedRequest, Box<RunReport>> {
        let metadata = self.report_metadata_for_surface(surface);
        let metrics = RequestMetrics {
            source_bytes,
            ..RequestMetrics::default()
        };

        if source_bytes > self.max_source_bytes {
            return Err(Box::new(request_report_error(
                RequestError::SourceTooLarge {
                    bytes: source_bytes,
                    cap: self.max_source_bytes,
                },
                metrics,
                metadata,
                tenant,
            )));
        }
        let reservation = match self.resource_accounting.try_reserve(
            tenant,
            self.aggregate_limits,
            self.aggregate_reservation(source_bytes, budget),
        ) {
            Ok(reservation) => reservation,
            Err(error) => {
                return Err(Box::new(request_report_error(
                    error, metrics, metadata, tenant,
                )));
            }
        };

        let ingress = match self.ingress.try_enter(tenant) {
            Ok(guard) => guard,
            Err(error) => {
                return Err(Box::new(request_report_error(
                    error, metrics, metadata, tenant,
                )));
            }
        };
        Ok(AdmittedRequest {
            metadata,
            metrics,
            ingress,
            reservation,
        })
    }

    async fn run_preflight_pipeline(
        &self,
        surface: &Arc<Surface>,
        source: &[u8],
        budget: &RunControl,
        tenant: TenantId,
        metadata: RunMetadata,
        metrics: &mut RequestMetrics,
    ) -> Result<Arc<BytecodeChunk>, Box<RunReport>> {
        let engine = PreflightEngine {
            surface: Arc::clone(surface),
            compile_policy: self.compile_policy.clone(),
            limits: self.preflight,
        };
        let cache_key = preflight_cache_key(surface, source, &self.compile_policy_fingerprint);
        if let Some(verdict) = self.preflight_cache.get(&cache_key) {
            metrics.parse_ast_nodes = verdict.ast_nodes;
            metrics.type_arena_nodes = verdict.type_arena_nodes;
            if let Err(error) = engine.enforce_limit(
                PreflightStage::Parse,
                PreflightLimit::ParseAstNodes,
                verdict.ast_nodes,
                self.preflight.max_parse_ast_nodes,
            ) {
                return Err(Box::new(request_report_error(
                    error.into_request_error(),
                    *metrics,
                    metadata,
                    tenant,
                )));
            }
            if let Err(error) = engine.enforce_limit(
                PreflightStage::TypeCheck,
                PreflightLimit::ArenaNodes,
                verdict.type_arena_nodes,
                self.preflight.max_type_arena_nodes,
            ) {
                return Err(Box::new(request_report_error(
                    error.into_request_error(),
                    *metrics,
                    metadata,
                    tenant,
                )));
            }
            return match verdict.outcome {
                PreflightOutcome::TypeErrors(diagnostics) => Err(Box::new(request_report_error(
                    RequestError::TypeErrors(diagnostics),
                    *metrics,
                    metadata,
                    tenant,
                ))),
                PreflightOutcome::Chunk(chunk) => {
                    let bytecode = compiled_bytecode_metrics(&chunk).map_err(|error| {
                        Box::new(request_report_error(
                            error,
                            *metrics,
                            metadata.clone(),
                            tenant,
                        ))
                    })?;
                    engine
                        .enforce_limit(
                            PreflightStage::Compile,
                            PreflightLimit::CompiledInstructions,
                            bytecode.instructions,
                            self.preflight.max_compiled_instructions,
                        )
                        .and_then(|_| {
                            engine.enforce_limit(
                                PreflightStage::Compile,
                                PreflightLimit::CompiledBytecodeBytes,
                                bytecode.encoded_bytes,
                                self.preflight.max_compiled_bytecode_bytes,
                            )
                        })
                        .map_err(|error| {
                            Box::new(request_report_error(
                                error.into_request_error(),
                                *metrics,
                                metadata,
                                tenant,
                            ))
                        })?;
                    metrics.compiled_instructions = bytecode.instructions;
                    metrics.compiled_bytecode_bytes = bytecode.encoded_bytes;
                    Ok(chunk)
                }
            };
        }

        let shared_source: Arc<[u8]> = Arc::from(source);
        let parse_source = Arc::clone(&shared_source);
        let parse_engine = engine.clone();
        let parsed = run_preflight_stage(
            budget,
            "parse-budget",
            Arc::clone(&self.preflight_permits),
            move |cancel| {
                parse_engine
                    .parse(parse_source, Some(&cancel))
                    .map_err(PreflightEngineError::into_request_error)
            },
        )
        .await;
        metrics.parse_time = parsed.elapsed;
        let parsed = match parsed.result {
            Ok(parsed) => parsed,
            Err(error) => {
                return Err(Box::new(request_report_error(
                    error, *metrics, metadata, tenant,
                )));
            }
        };
        metrics.parse_ast_nodes = parsed.ast_nodes();

        let check_parsed = parsed;
        let check_engine = engine.clone();
        let check_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let checker_cancel = Arc::clone(&check_cancel);
        let check = run_async_preflight_stage(
            budget,
            "type-check",
            Arc::clone(&self.preflight_permits),
            check_cancel,
            async move {
                check_engine
                    .check_async(check_parsed, Some(checker_cancel))
                    .await
                    .map_err(PreflightEngineError::into_request_error)
            },
        )
        .await;
        metrics.check_time = check.elapsed;
        let check = match check.result {
            Ok(result) => result,
            Err(error) => {
                return Err(Box::new(request_report_error(
                    error, *metrics, metadata, tenant,
                )));
            }
        };
        let checked = match check {
            PreflightCheckOutcome::Accepted(checked) => {
                metrics.type_arena_nodes = checked.type_arena_nodes;
                checked
            }
            PreflightCheckOutcome::TypeErrors {
                diagnostics,
                ast_nodes,
                type_arena_nodes,
            } => {
                metrics.parse_ast_nodes = ast_nodes;
                metrics.type_arena_nodes = type_arena_nodes;
                self.preflight_cache.insert(
                    cache_key,
                    PreflightVerdict {
                        ast_nodes,
                        type_arena_nodes,
                        retained_bytes: serde_json::to_vec(&diagnostics)
                            .map_or(0, |encoded| encoded.len()),
                        outcome: PreflightOutcome::TypeErrors(diagnostics.clone()),
                    },
                );
                return Err(Box::new(request_report_error(
                    RequestError::TypeErrors(diagnostics),
                    *metrics,
                    metadata,
                    tenant,
                )));
            }
        };

        let compile_engine = engine.clone();
        let compiled_caps = engine.default_compiled_caps();
        let outcome = run_preflight_stage(
            budget,
            "compile",
            Arc::clone(&self.preflight_permits),
            move |cancel| {
                compile_engine
                    .compile(checked, Some(cancel), compiled_caps)
                    .map_err(PreflightEngineError::into_request_error)
            },
        )
        .await;
        metrics.compile_time = outcome.elapsed;
        let outcome = match outcome.result {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(Box::new(request_report_error(
                    error, *metrics, metadata, tenant,
                )));
            }
        };
        metrics.parse_ast_nodes = outcome.ast_nodes;
        metrics.type_arena_nodes = outcome.type_arena_nodes;
        metrics.compiled_instructions = outcome.bytecode.instructions;
        metrics.compiled_bytecode_bytes = outcome.bytecode.encoded_bytes;
        debug_assert!(!outcome.diagnostics.has_issues());
        let chunk = Arc::new(outcome.chunk);
        self.preflight_cache.insert(
            cache_key,
            PreflightVerdict {
                ast_nodes: metrics.parse_ast_nodes,
                type_arena_nodes: metrics.type_arena_nodes,
                retained_bytes: metrics.compiled_bytecode_bytes,
                outcome: PreflightOutcome::Chunk(Arc::clone(&chunk)),
            },
        );
        Ok(chunk)
    }

    fn aggregate_reservation(
        &self,
        source_bytes: usize,
        budget: &RunControl,
    ) -> AccountedResources {
        let remaining = budget.deadline.saturating_duration_since(Instant::now());
        let mut reservation = AccountedResources {
            requests: 1,
            source_bytes: u64::try_from(source_bytes).unwrap_or(u64::MAX),
            ..AccountedResources::default()
        };
        if let Some(cap) = self.aggregate_limits.max_preflight_time {
            reservation.preflight_time = remaining.min(cap);
        }
        if let Some(cap) = self.aggregate_limits.max_run_time {
            reservation.run_time = remaining.min(cap);
        }
        if let Some(cap) = self.aggregate_limits.max_gas_spent {
            reservation.gas_spent = self.base_limits.gas.unwrap_or(cap).min(cap);
        }
        if let Some(cap) = self.aggregate_limits.max_charged_bytes {
            let bytecode =
                u64::try_from(self.preflight.max_compiled_bytecode_bytes).unwrap_or(u64::MAX);
            let heap = self
                .base_limits
                .max_memory_bytes
                .map(|bytes| u64::try_from(bytes).unwrap_or(u64::MAX))
                .unwrap_or(cap);
            reservation.charged_bytes = reservation
                .source_bytes
                .saturating_add(bytecode)
                .saturating_add(heap)
                .min(cap);
        }
        reservation
    }

    fn finalize_admitted_report(
        &self,
        report: RunReport,
        reservation: TenantResourceReservation,
    ) -> RunReport {
        if should_record_report_resources(&report) {
            reservation.settle(&report.metrics);
        }
        report
    }

    pub(crate) fn runtime_compiler_for_surface(
        &self,
        surface: &Arc<Surface>,
    ) -> Arc<dyn RuntimeCompiler> {
        #[cfg(any())]
        if let Some(compiler) = &self.runtime_compiler_override {
            return Arc::clone(compiler);
        }
        Arc::new(ExecutorRuntimeCompiler {
            surface: Arc::clone(surface),
            max_source_bytes: self.max_source_bytes,
            compile_policy: self.compile_policy.clone(),
            preflight: self.preflight,
        })
    }
}

fn should_record_report_resources(report: &RunReport) -> bool {
    !(report.metrics.parse_time == Duration::ZERO
        && report.metrics.check_time == Duration::ZERO
        && report.metrics.compile_time == Duration::ZERO
        && report.metrics.vm_build_time == Duration::ZERO
        && report.metrics.sandbox_time == Duration::ZERO
        && report.metrics.load_time == Duration::ZERO
        && report.metrics.run_time == Duration::ZERO)
}

struct AdmittedRequest {
    metadata: RunMetadata,
    metrics: RequestMetrics,
    ingress: IngressGuard,
    reservation: TenantResourceReservation,
}

struct LaneRequest {
    surface: Arc<Surface>,
    ambient: Ambient,
    limits: Limits,
    runtime_compiler: Arc<dyn RuntimeCompiler>,
    chunk: Arc<BytecodeChunk>,
    exec_cancel: Cancel,
    metrics: RequestMetrics,
    accounting: LaneAccounting,
}

struct LaneAccounting {
    reservation: Option<TenantResourceReservation>,
    ingress: Option<IngressGuard>,
    baseline: RequestMetrics,
}

impl LaneAccounting {
    fn new(
        reservation: TenantResourceReservation,
        ingress: IngressGuard,
        baseline: RequestMetrics,
    ) -> Self {
        Self {
            reservation: Some(reservation),
            ingress: Some(ingress),
            baseline,
        }
    }

    fn settle(mut self, metrics: &RequestMetrics) {
        if let Some(reservation) = self.reservation.take() {
            reservation.settle(metrics);
        }
        self.ingress.take();
    }
}

impl Drop for LaneAccounting {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            reservation.settle(&self.baseline);
        }
    }
}

struct LaneRequestResult {
    metrics: RequestMetrics,
    outcome: Result<Vec<ruau_vm::ValueSnapshot>, RequestError>,
}

async fn run_request_vm_on_lane(request: LaneRequest) -> LaneRequestResult {
    let LaneRequest {
        surface,
        ambient,
        limits,
        runtime_compiler,
        chunk,
        exec_cancel,
        metrics,
        accounting,
    } = request;
    let result = run_request_vm_inner(LaneVmRequest {
        surface,
        ambient,
        limits,
        runtime_compiler,
        chunk,
        exec_cancel,
        metrics,
    })
    .await;
    accounting.settle(&result.metrics);
    result
}

struct LaneVmRequest {
    surface: Arc<Surface>,
    ambient: Ambient,
    limits: Limits,
    runtime_compiler: Arc<dyn RuntimeCompiler>,
    chunk: Arc<BytecodeChunk>,
    exec_cancel: Cancel,
    metrics: RequestMetrics,
}

async fn run_request_vm_inner(request: LaneVmRequest) -> LaneRequestResult {
    let LaneVmRequest {
        surface,
        ambient,
        limits,
        runtime_compiler,
        chunk,
        exec_cancel,
        mut metrics,
    } = request;

    if exec_cancel.is_cancelled() {
        return LaneRequestResult {
            metrics,
            outcome: Err(request_error_from_stop(
                exec_cancel.stop_reason().unwrap_or(StopReason::Cancelled),
            )),
        };
    }

    let started = Instant::now();
    let builder = surface
        .vm_builder(&VmConfig::untrusted(ambient, limits))
        .runtime_compiler(runtime_compiler);
    // The executor build validates that every lane submission carries ambient,
    // limits, and runtime capabilities, so the fail-closed VM builder cannot
    // reject it here.
    let (mut vm, sandbox_time) = match builder.build_with_sandbox_timing() {
        Ok(built) => built,
        Err(ruau_vm::VmBuildError::Sandbox(_)) => {
            metrics.vm_build_time = started.elapsed();
            return LaneRequestResult {
                metrics,
                outcome: Err(RequestError::SandboxFailed),
            };
        }
        Err(error) => panic!("executor sets ambient, limits, and runtime capabilities: {error}"),
    };
    let total_build_time = started.elapsed();
    metrics.sandbox_time = sandbox_time;
    metrics.vm_build_time = total_build_time.saturating_sub(sandbox_time);

    let started = Instant::now();
    let module = match vm.load(&chunk) {
        Ok(module) => module,
        Err(error) => {
            metrics.load_time = started.elapsed();
            record_vm_metrics(&mut metrics, &vm);
            return LaneRequestResult {
                metrics,
                outcome: Err(map_load_error(error)),
            };
        }
    };
    metrics.load_time = started.elapsed();

    let started = Instant::now();
    let outcome = vm.exec_async(&module, CallOptions::new()).await;
    metrics.run_time = started.elapsed();

    record_vm_metrics(&mut metrics, &vm);
    let outcome = match outcome {
        Ok(values) => Ok(values),
        Err(error) => Err(map_exec_error(error)),
    };
    LaneRequestResult { metrics, outcome }
}

fn request_error_from_stop(reason: StopReason) -> RequestError {
    match reason {
        StopReason::Cancelled => RequestError::Cancelled,
        StopReason::Deadline => RequestError::DeadlineExceeded,
    }
}

fn request_error_from_cancel(cancel: &Cancel) -> RequestError {
    request_error_from_stop(cancel.stop_reason().unwrap_or(StopReason::Cancelled))
}

fn record_vm_metrics(metrics: &mut RequestMetrics, vm: &Vm) {
    metrics.heap_bytes = vm.heap_used_bytes();
    metrics.peak_heap_bytes = vm.peak_heap_bytes();
    metrics.gc_cycles = vm.gc_cycles();
    metrics.gas_spent = vm.gas_spent();
    metrics.vm_execution_count = vm.execution_count();
}

pub fn map_unwind_error(kind: RuntimeErrorKind, rendered: ruau_vm::ValueSnapshot) -> RequestError {
    match kind {
        RuntimeErrorKind::Cancelled => RequestError::Cancelled,
        RuntimeErrorKind::Deadline => RequestError::DeadlineExceeded,
        RuntimeErrorKind::Memory => RequestError::OutOfMemory(rendered),
        RuntimeErrorKind::PanicPoison => RequestError::PanicPoison(rendered),
        RuntimeErrorKind::HandlerFailure => RequestError::HandlerFailure(rendered),
        RuntimeErrorKind::UnresolvedRequire => RequestError::Runtime(rendered),
        RuntimeErrorKind::Runtime => RequestError::Runtime(rendered),
    }
}

fn map_exec_error(error: ExecError) -> RequestError {
    match error {
        ExecError::Script(error) => map_unwind_error(error.kind(), error.value().clone()),
        ExecError::Stopped(reason) => request_error_from_stop(reason),
        ExecError::PanicPoison => {
            RequestError::PanicPoison(ruau_vm::ValueSnapshot::String(b"VM is poisoned".to_vec()))
        }
        ExecError::Entry { message } => {
            RequestError::Runtime(ruau_vm::ValueSnapshot::String(message.into_bytes()))
        }
        ExecError::Marshal { message } => RequestError::Runtime(ruau_vm::ValueSnapshot::String(
            format!("result marshal failed: {message}").into_bytes(),
        )),
    }
}

pub fn map_load_error(error: LoadError) -> RequestError {
    match error {
        LoadError::OutOfMemory => RequestError::OutOfMemory(ruau_vm::ValueSnapshot::String(
            b"out of memory loading bytecode".to_vec(),
        )),
        other => RequestError::Load(other),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompiledBytecodeMetrics {
    instructions: usize,
    encoded_bytes: usize,
}

struct ExecutorRuntimeCompiler {
    surface: Arc<Surface>,
    max_source_bytes: usize,
    compile_policy: CompileOptions,
    preflight: PreflightLimits,
}

impl RuntimeCompiler for ExecutorRuntimeCompiler {
    fn compile(
        &self,
        source: &[u8],
        context: RuntimeCompileContext,
    ) -> Result<BytecodeChunk, Vec<u8>> {
        let cancellation = context.cancellation_flag();
        context.check_cancelled()?;
        let limits = context.limits;
        enforce_executor_runtime_compile_limit(
            "source byte",
            source.len(),
            limits.max_source_bytes.min(self.max_source_bytes),
        )?;
        context.check_cancelled()?;

        let engine = PreflightEngine {
            surface: Arc::clone(&self.surface),
            compile_policy: self.compile_policy.clone(),
            limits: self.preflight,
        };
        let identity = if context.module_id.is_none() && std::str::from_utf8(source).is_err() {
            ReadyPreflightSourceIdentity::Sourceless
        } else {
            ReadyPreflightSourceIdentity::Runtime(context.module_id)
        };

        let compiled_caps = CompiledBytecodeMetrics {
            instructions: limits
                .max_compiled_instructions
                .min(self.preflight.max_compiled_instructions),
            encoded_bytes: limits
                .max_compiled_bytecode_bytes
                .min(self.preflight.max_compiled_bytecode_bytes),
        };
        let cancel = cancellation.as_ref().map(CancellationFlag::flag);
        let outcome = engine
            .run_ready(Arc::from(source), identity, cancel.as_ref(), compiled_caps)
            .map_err(PreflightEngineError::into_runtime_message)?;
        Ok(outcome.chunk)
    }
}

fn runtime_compilation_cancelled() -> Vec<u8> {
    b"runtime compilation cancelled".to_vec()
}

fn enforce_executor_runtime_compile_limit(
    label: &str,
    used: usize,
    cap: usize,
) -> Result<(), Vec<u8>> {
    if used > cap {
        return Err(
            format!("runtime compilation {label} limit exceeded: {used} > {cap}").into_bytes(),
        );
    }
    Ok(())
}

fn compiled_bytecode_metrics(
    chunk: &BytecodeChunk,
) -> Result<CompiledBytecodeMetrics, RequestError> {
    let instructions = match chunk {
        BytecodeChunk::Valid { protos, .. } => protos
            .iter()
            .map(|proto| {
                proto
                    .code
                    .iter()
                    .map(|instruction| instruction.word_len() as usize)
                    .sum::<usize>()
            })
            .sum(),
        BytecodeChunk::Error { .. } => 0,
    };
    let encoded_bytes = encode_chunk(chunk).map_err(|error| {
        RequestError::Runtime(ruau_vm::ValueSnapshot::String(
            format!("source preflight compile product failed to encode: {error}").into_bytes(),
        ))
    })?;
    Ok(CompiledBytecodeMetrics {
        instructions,
        encoded_bytes: encoded_bytes.len(),
    })
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("surface", &self.surface)
            .field("ambient", &self.ambient)
            .field("features", &self.features)
            .field("max_source_bytes", &self.max_source_bytes)
            .field("preflight", &self.preflight)
            .field("ingress", &self.ingress.limits)
            .field("aggregate_limits", &self.aggregate_limits)
            .field("lane_pool", &self.lane_pool.metrics())
            .finish_non_exhaustive()
    }
}
fn check_preflight_budget(budget: &RunControl) -> Result<(), RequestError> {
    if budget.cancel.is_cancelled() {
        return Err(request_error_from_cancel(&budget.cancel));
    }
    if Instant::now() >= budget.deadline {
        budget.cancel.stop(StopReason::Deadline);
        return Err(RequestError::DeadlineExceeded);
    }
    Ok(())
}

/// Default cap for concurrent type-check stages: the host parallelism,
/// clamped so a wide machine cannot dedicate every core to untrusted checks.
pub fn default_type_check_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
        .clamp(1, 8)
}

#[derive(Debug)]
pub struct TimedPreflightStage<T> {
    pub result: Result<T, RequestError>,
    pub elapsed: Duration,
}

pub async fn run_preflight_stage<T>(
    budget: &RunControl,
    stage: &'static str,
    permits: Arc<tokio::sync::Semaphore>,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<T, RequestError> + Send + 'static,
) -> TimedPreflightStage<T>
where
    T: Send + 'static,
{
    if let Err(error) = check_preflight_budget(budget) {
        return TimedPreflightStage {
            result: Err(error),
            elapsed: Duration::ZERO,
        };
    }
    let deadline = tokio::time::sleep_until(tokio::time::Instant::from_std(budget.deadline));
    tokio::pin!(deadline);
    // Bounded admission: wait for a preflight CPU slot, still honoring
    // cancellation and the deadline while queued.
    let permit = tokio::select! {
        biased;
        () = budget.cancel.cancelled() => return TimedPreflightStage {
            result: Err(request_error_from_cancel(&budget.cancel)),
            elapsed: Duration::ZERO,
        },
        () = &mut deadline => {
            budget.cancel.stop(StopReason::Deadline);
            return TimedPreflightStage {
                result: Err(RequestError::DeadlineExceeded),
                elapsed: Duration::ZERO,
            };
        },
        permit = permits.acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(_) => {
                return TimedPreflightStage {
                    result: Err(RequestError::Runtime(ruau_vm::ValueSnapshot::String(
                        format!("source preflight {stage} pool is closed").into_bytes(),
                    ))),
                    elapsed: Duration::ZERO,
                };
            }
        },
    };
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel_flag);
    let started = Instant::now();
    let handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work(worker_cancel)
    });
    let result = tokio::select! {
        biased;
        () = budget.cancel.cancelled() => {
            cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            Err(request_error_from_cancel(&budget.cancel))
        },
        () = &mut deadline => {
            budget.cancel.stop(StopReason::Deadline);
            cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            Err(RequestError::DeadlineExceeded)
        },
        joined = handle => match joined {
            Ok(result) => result,
            Err(error) if error.is_panic() => Err(RequestError::PanicPoison(ruau_vm::ValueSnapshot::String(
                format!("source preflight {stage} task panicked").into_bytes(),
            ))),
            Err(_) => Err(RequestError::Runtime(ruau_vm::ValueSnapshot::String(
                format!("source preflight {stage} task was cancelled").into_bytes(),
            ))),
        },
    };
    TimedPreflightStage {
        result,
        elapsed: started.elapsed(),
    }
}

enum AsyncPreflightStageResult<T> {
    Finished(Result<T, RequestError>),
    Panic,
}

pub struct PreflightAsyncRuntimePool {
    runtimes: Mutex<Vec<tokio::runtime::Runtime>>,
}

struct PreflightAsyncRuntimeLease {
    pool: Arc<PreflightAsyncRuntimePool>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl PreflightAsyncRuntimePool {
    fn new(size: usize) -> Result<Self, String> {
        let mut runtimes = Vec::with_capacity(size);
        for _ in 0..size {
            runtimes.push(build_preflight_current_thread_runtime()?);
        }
        Ok(Self {
            runtimes: Mutex::new(runtimes),
        })
    }

    fn lease(self: &Arc<Self>) -> Result<PreflightAsyncRuntimeLease, String> {
        let runtime = self
            .runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .map_or_else(build_preflight_current_thread_runtime, Ok)?;
        Ok(PreflightAsyncRuntimeLease {
            pool: Arc::clone(self),
            runtime: Some(runtime),
        })
    }
}

impl PreflightAsyncRuntimeLease {
    fn block_on<F: std::future::Future>(&mut self, future: F) -> F::Output {
        self.runtime
            .as_mut()
            .expect("preflight runtime lease always owns a runtime")
            .block_on(future)
    }
}

impl Drop for PreflightAsyncRuntimeLease {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            self.pool
                .runtimes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(runtime);
        }
    }
}

fn build_preflight_current_thread_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

pub fn preflight_async_runtime() -> Result<Arc<PreflightAsyncRuntimePool>, String> {
    FRONT_DOOR_ASYNC_RUNTIME
        .get_or_init(|| {
            PreflightAsyncRuntimePool::new(default_type_check_concurrency()).map(Arc::new)
        })
        .clone()
}

pub async fn run_async_preflight_stage<T, F>(
    budget: &RunControl,
    stage: &'static str,
    permits: Arc<tokio::sync::Semaphore>,
    cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    work: F,
) -> TimedPreflightStage<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, RequestError>> + Send + 'static,
{
    if let Err(error) = check_preflight_budget(budget) {
        return TimedPreflightStage {
            result: Err(error),
            elapsed: Duration::ZERO,
        };
    }
    let deadline = tokio::time::sleep_until(tokio::time::Instant::from_std(budget.deadline));
    tokio::pin!(deadline);
    // Bounded admission: wait for a stage slot, still honoring cancellation
    // and the deadline while queued.
    let permit = tokio::select! {
        biased;
        () = budget.cancel.cancelled() => return TimedPreflightStage {
            result: Err(request_error_from_cancel(&budget.cancel)),
            elapsed: Duration::ZERO,
        },
        () = &mut deadline => {
            budget.cancel.stop(StopReason::Deadline);
            return TimedPreflightStage {
                result: Err(RequestError::DeadlineExceeded),
                elapsed: Duration::ZERO,
            };
        },
        permit = permits.acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(_) => {
                return TimedPreflightStage {
                    result: Err(RequestError::Runtime(ruau_vm::ValueSnapshot::String(
                        format!("source preflight {stage} pool is closed").into_bytes(),
                    ))),
                    elapsed: Duration::ZERO,
                };
            }
        },
    };
    let started = Instant::now();
    let runtime = preflight_async_runtime().map_err(|error| {
        RequestError::Runtime(ruau_vm::ValueSnapshot::String(
            format!("source preflight {stage} async runtime failed: {error}").into_bytes(),
        ))
    });
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            return TimedPreflightStage {
                result: Err(error),
                elapsed: started.elapsed(),
            };
        }
    };
    let runtime = runtime.lease().map_err(|error| {
        RequestError::Runtime(ruau_vm::ValueSnapshot::String(
            format!("source preflight {stage} async runtime failed: {error}").into_bytes(),
        ))
    });
    let mut runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            return TimedPreflightStage {
                result: Err(error),
                elapsed: started.elapsed(),
            };
        }
    };
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                tokio::select! {
                    result = work => result,
                    _ = cancel_rx => Err(RequestError::Cancelled),
                }
            })
        }));
        match result {
            Ok(result) => AsyncPreflightStageResult::Finished(result),
            Err(_) => AsyncPreflightStageResult::Panic,
        }
    });

    let result = tokio::select! {
        biased;
        () = budget.cancel.cancelled() => {
            // Stop the abandoned worker: the flag halts CPU-bound loops (the
            // constraint solver polls it) and dropping the sender wakes the
            // worker's select at its next await point.
            cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            drop(cancel_tx);
            Err(request_error_from_cancel(&budget.cancel))
        }
        () = &mut deadline => {
            budget.cancel.stop(StopReason::Deadline);
            cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            drop(cancel_tx);
            Err(RequestError::DeadlineExceeded)
        }
        joined = handle => match joined {
            Ok(AsyncPreflightStageResult::Finished(result)) => result,
            Ok(AsyncPreflightStageResult::Panic) => Err(RequestError::PanicPoison(ruau_vm::ValueSnapshot::String(
                format!("source preflight {stage} task panicked").into_bytes(),
            ))),
            Err(_) => Err(RequestError::Runtime(ruau_vm::ValueSnapshot::String(
                format!("source preflight {stage} task was cancelled").into_bytes(),
            ))),
        },
    };
    TimedPreflightStage {
        result,
        elapsed: started.elapsed(),
    }
}
