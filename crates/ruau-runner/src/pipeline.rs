use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use ruau_analysis::resolve::config::EmptyResolver;
use ruau_ast::{
    parse::{Options, SyntaxFlags, parse_file_bytes_with},
    syntax::{Expr, Local, Stat, Type, TypePack},
    visit::{NodePath, Visitor, WalkControl, walk_stat},
};
use ruau_bytecode::{BytecodeChunk, CompileErrorKind, CompileOptions, encode_chunk};
use ruau_source::{ModuleId, ModuleSource, RootOverlaySource};
use ruau_surface::{Surface, VmConfig};
use ruau_typecheck::{checker::Config, diagnostics::Diagnostics, frontend::GraphChecker};
use ruau_vm::{
    Ambient, CallOptions, Cancel, Deadline, ExecError, ExecutionFeatures, Limits, LoadError,
    RuntimeCompileContext, RuntimeCompiler, Vm,
};
use ruau_vm_api::RuntimeErrorKind;

use super::{
    TenantId,
    admission::{
        IngressAdmission, IngressGuard, TenantResourceAccounting, TenantResourceReservation,
    },
    budget::Budget,
    builder::Builder,
    front_door::{FrontDoorCache, FrontDoorOutcome, FrontDoorVerdict},
    render::{request_report_error, request_report_success},
    types::{
        AggregateResourceLimits, FrontDoorLimit, FrontDoorLimits, FrontDoorStage, RequestError,
        RequestMetrics, RequestOutcome, RequestReport, RequestReportMetadata, ResultValue,
        TenantResourceTotals,
    },
};
use crate::lanes::{LaneMetrics, LanePool};

pub const DEFAULT_REQUEST_QUANTUM: u64 = 4_096;
const RUNTIME_COMPILE_MODULE_ID: &str = "__runner_runtime_compile__";
static EMPTY_CONFIG_RESOLVER: EmptyResolver = EmptyResolver;
const RUNNER_REQUEST_MODULE_ID: &str = "__runner_request__";
static FRONT_DOOR_ASYNC_RUNTIME: OnceLock<Result<Arc<FrontDoorAsyncRuntimePool>, String>> =
    OnceLock::new();

#[derive(Debug)]
struct SourceFrontDoorCheck {
    has_issues: bool,
    diagnostics: Diagnostics,
    type_arena_nodes: usize,
}

fn checked_frontend_for_root<'source>(
    surface: &Surface,
    source: &'source RootOverlaySource<'source>,
    parse_options: Options,
    syntax_flags: SyntaxFlags,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> GraphChecker<'source> {
    let mut checker = surface.new_checker();
    if let Some(cancel) = cancel {
        checker.set_cancel_flag(cancel);
    }
    let mut frontend = GraphChecker::with_checker(source, &EMPTY_CONFIG_RESOLVER, checker);
    frontend.set_parse_options(parse_options);
    frontend.set_syntax_flags(syntax_flags);
    frontend.set_source_mode_override(Some(surface.analysis_mode()));
    frontend
}

fn source_front_door_check_from_frontend(
    frontend: &GraphChecker<'_>,
    result: &ruau_analysis::ParseGraphResult,
    max_type_diagnostics: usize,
) -> SourceFrontDoorCheck {
    let diagnostics = checked_source_graph_diagnostics(frontend, result);
    let has_issues = diagnostics.has_issues();
    let diagnostics = diagnostics.capped(max_type_diagnostics);
    SourceFrontDoorCheck {
        has_issues,
        diagnostics,
        type_arena_nodes: frontend.checker().arena().type_len()
            + frontend.checker().arena().pack_len(),
    }
}

fn check_sourceless_source_bytes(
    surface: &Surface,
    source: &[u8],
    parse_options: Options,
    syntax_flags: SyntaxFlags,
    max_type_diagnostics: usize,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> SourceFrontDoorCheck {
    let mut checker = surface.new_checker();
    if let Some(cancel) = cancel {
        checker.set_cancel_flag(cancel);
    }
    let mut config = Config::with_source_mode(surface.analysis_mode());
    config.parse_options = parse_options;
    config.syntax_flags = syntax_flags;
    let checked = checker.check_source_bytes_with_config(source, config);
    let has_issues = checked.has_issues();
    let diagnostics = checked.diagnostics().clone().capped(max_type_diagnostics);
    SourceFrontDoorCheck {
        has_issues,
        diagnostics,
        type_arena_nodes: checker.arena().type_len() + checker.arena().pack_len(),
    }
}

async fn check_root_source_async(
    surface: &Surface,
    source: Vec<u8>,
    module_source: Option<&dyn ModuleSource>,
    parse_options: Options,
    syntax_flags: SyntaxFlags,
    max_type_diagnostics: usize,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> SourceFrontDoorCheck {
    let mut source =
        RootOverlaySource::new(ModuleId::canonicalized(RUNNER_REQUEST_MODULE_ID), source)
            .with_display_name("request")
            .reject_delegate_root_id_collision(true);
    if let Some(module_source) = module_source {
        source = source.with_delegate(module_source);
    }
    let root = source.root_name();
    let mut frontend =
        checked_frontend_for_root(surface, &source, parse_options, syntax_flags, cancel);
    let result = frontend.check_async(root).await;
    source_front_door_check_from_frontend(&frontend, &result, max_type_diagnostics)
}

fn check_runtime_source_ready(
    surface: &Surface,
    source: &[u8],
    module_id: Option<ModuleId>,
    parse_options: Options,
    syntax_flags: SyntaxFlags,
    max_type_diagnostics: usize,
    cancel: Option<Arc<AtomicBool>>,
) -> SourceFrontDoorCheck {
    let module_source = surface.module_source();
    let root_id = module_id
        .as_ref()
        .and_then(|id| id.as_str().map(ModuleId::canonicalized))
        .unwrap_or_else(|| ModuleId::canonicalized(RUNTIME_COMPILE_MODULE_ID));
    let mut source =
        RootOverlaySource::new(root_id, source.to_vec()).with_display_name("runtime compilation");
    if let Some(module_id) = module_id {
        source = source.with_root_requester(module_id);
    }
    if let Some(module_source) = module_source.as_deref() {
        source = source.with_delegate(module_source);
    }
    let root = source.root_name();
    let mut frontend =
        checked_frontend_for_root(surface, &source, parse_options, syntax_flags, cancel);
    let result = frontend.check(root);
    source_front_door_check_from_frontend(&frontend, &result, max_type_diagnostics)
}

#[cfg(any())]
pub fn runtime_source_check_cancelled_for_test(surface: &Surface, source: &[u8]) -> bool {
    let cancel = Arc::new(AtomicBool::new(true));
    check_runtime_source_ready(
        surface,
        source,
        None,
        Options::default(),
        SyntaxFlags::default(),
        usize::MAX,
        Some(cancel),
    )
    .has_issues
}

async fn check_source_graph_for_surface(
    surface: &Surface,
    source: Vec<u8>,
    parse_options: Options,
    syntax_flags: SyntaxFlags,
    max_type_diagnostics: usize,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> (bool, Diagnostics, usize) {
    let module_source = surface.module_source();
    if module_source.is_none() && std::str::from_utf8(&source).is_err() {
        let check = check_sourceless_source_bytes(
            surface,
            &source,
            parse_options,
            syntax_flags,
            max_type_diagnostics,
            cancel,
        );
        return (check.has_issues, check.diagnostics, check.type_arena_nodes);
    }
    let check = check_root_source_async(
        surface,
        source,
        module_source.as_deref(),
        parse_options,
        syntax_flags,
        max_type_diagnostics,
        cancel,
    )
    .await;
    (check.has_issues, check.diagnostics, check.type_arena_nodes)
}

fn checked_source_graph_diagnostics(
    frontend: &GraphChecker<'_>,
    result: &ruau_analysis::ParseGraphResult,
) -> Diagnostics {
    frontend.graph_diagnostics(result).into_flat_diagnostics()
}

/// Bounded request runner built from shared configuration.
///
/// Per-request source and budget are passed to [`Runner::run`].
pub struct Runner {
    pub(super) surface: Surface,
    pub(super) ambient: Ambient,
    pub(super) base_limits: Limits,
    pub(super) features: ExecutionFeatures,
    pub(super) max_source_bytes: usize,
    pub(super) compile_policy: CompileOptions,
    #[cfg(any())]
    pub(crate) front_door: FrontDoorLimits,
    #[allow(clippy::cfg_not_test)] // production visibility; tests use the `pub(crate)` field above
    #[cfg(not(any()))]
    pub(super) front_door: FrontDoorLimits,
    pub(super) ingress: Arc<IngressAdmission>,
    pub(super) aggregate_limits: AggregateResourceLimits,
    pub(super) resource_accounting: Arc<TenantResourceAccounting>,
    pub(super) lane_pool: LanePool,
    /// Bounded admission for CPU-heavy parser/checker/compiler work.
    pub(super) front_door_permits: Arc<tokio::sync::Semaphore>,
    pub(super) front_door_cache: FrontDoorCache,
    #[cfg(any())]
    pub(crate) runtime_compiler_override: Option<Arc<dyn RuntimeCompiler>>,
}
impl Runner {
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
    pub fn surface(&self) -> &Surface {
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
    pub fn report_metadata(&self) -> RequestReportMetadata {
        self.report_metadata_for_surface(&self.surface)
    }

    /// Static metadata for a request that uses `surface`.
    #[must_use]
    pub fn report_metadata_for_surface(&self, surface: &Surface) -> RequestReportMetadata {
        RequestReportMetadata {
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

    /// Number of dormant tenant accounting entries evicted to keep aggregate
    /// tracking bounded.
    #[must_use]
    pub fn tenant_accounting_evictions(&self) -> u64 {
        self.resource_accounting.evictions()
    }

    /// The number of worker lanes this runner dispatches VM work across.
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
    pub async fn run(&self, request: super::Request<'_>) -> Result<RequestOutcome, RequestError> {
        self.run_report(request).await.into_result()
    }

    /// Runs `request` and always returns a report, including failures.
    pub async fn run_report(&self, request: super::Request<'_>) -> RequestReport {
        let surface = request
            .surface
            .cloned()
            .unwrap_or_else(|| self.surface.clone());

        self.run_report_for_tenant_inner(request.tenant, surface, request.source, request.budget)
            .await
    }

    async fn run_report_for_tenant_inner(
        &self,
        tenant: TenantId,
        surface: Surface,
        source: &[u8],
        budget: Budget,
    ) -> RequestReport {
        let AdmittedRequest {
            metadata,
            mut metrics,
            _ingress,
            reservation,
        } = match self.admit_request(tenant, &surface, source.len(), &budget) {
            Ok(admitted) => admitted,
            Err(report) => return *report,
        };

        let chunk = match self
            .run_front_door_pipeline(
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

        if let Err(report) =
            self.enforce_compiled_limits(&chunk, &mut metrics, metadata.clone(), tenant)
        {
            return self.finalize_admitted_report(*report, reservation);
        }

        // 6. Move VM-owned work onto the lane pool. The runner task keeps the
        //    source cap, ingress guard, front-door work, request metadata, and
        //    deadline timer. The lane closure owns VM build/sandbox/load/run,
        //    rendering, and VM metrics because those all borrow the VM.
        let exec_cancel = budget.cancel.child();
        let mut limits = self.base_limits.clone();
        limits.deadline = Some(Deadline::Wall(budget.deadline));
        limits.cancel = Some(exec_cancel.clone());
        if limits.quantum.is_none() {
            limits.quantum = Some(DEFAULT_REQUEST_QUANTUM);
        }
        // 7. The VM's wall deadline only gates host awaits, so the runner task
        //    bridges the request deadline to cancellation before handing work to a
        //    lane. The lane polls that token at VM safepoints and also checks it
        //    before building a VM if the request waited in a ready queue.
        let deadline_timer = {
            let token = exec_cancel.clone();
            let at = tokio::time::Instant::from_std(budget.deadline);
            tokio::spawn(async move {
                tokio::time::sleep_until(at).await;
                token.cancel();
            })
        };
        let lane_surface = surface.clone();
        let lane_ambient = self.ambient;
        let lane_runtime_compiler = self.runtime_compiler_for_surface(&surface);
        let lane_deadline = budget.deadline;
        let lane_cancel = exec_cancel.clone();
        let lane_metrics = metrics;
        let Some(submission) = self
            .lane_pool
            .submit_cancellable(tenant, move || async move {
                let request = LaneRequest {
                    surface: lane_surface,
                    ambient: lane_ambient,
                    limits,
                    runtime_compiler: lane_runtime_compiler,
                    chunk,
                    deadline: lane_deadline,
                    exec_cancel: lane_cancel,
                    metrics: lane_metrics,
                };
                run_request_vm_on_lane(request).await
            })
        else {
            deadline_timer.abort();
            return self.finalize_admitted_report(
                request_report_error(
                    RequestError::LaneAdmissionRejected { tenant },
                    metrics,
                    metadata,
                    tenant,
                ),
                reservation,
            );
        };

        let lane_deadline =
            tokio::time::sleep_until(tokio::time::Instant::from_std(budget.deadline));
        tokio::pin!(lane_deadline);
        let lane_result = tokio::select! {
            biased;
            () = budget.cancel.cancelled() => {
                exec_cancel.cancel();
                deadline_timer.abort();
                return self.finalize_admitted_report(
                    request_report_error(RequestError::Cancelled, metrics, metadata, tenant),
                    reservation,
                );
            }
            () = &mut lane_deadline => {
                exec_cancel.cancel();
                deadline_timer.abort();
                return self.finalize_admitted_report(
                    request_report_error(
                        RequestError::DeadlineExceeded,
                        metrics,
                        metadata,
                        tenant,
                    ),
                    reservation,
                );
            }
            result = submission.recv() => match result {
                Ok(result) => result,
                Err(_) => {
                    deadline_timer.abort();
                    return self.finalize_admitted_report(
                        request_report_error(
                            RequestError::Runtime(ResultValue::String(
                                b"lane pool dropped request".to_vec(),
                            )),
                            metrics,
                            metadata,
                            tenant,
                        ),
                        reservation,
                    );
                }
            },
        };
        deadline_timer.abort();

        let report = match lane_result.outcome {
            Ok(values) => request_report_success(values, lane_result.metrics, metadata, tenant),
            Err(error) => request_report_error(error, lane_result.metrics, metadata, tenant),
        };
        self.finalize_admitted_report(report, reservation)
    }

    fn admit_request(
        &self,
        tenant: TenantId,
        surface: &Surface,
        source_bytes: usize,
        budget: &Budget,
    ) -> Result<AdmittedRequest, Box<RequestReport>> {
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
            _ingress: ingress,
            reservation,
        })
    }

    async fn run_front_door_pipeline(
        &self,
        surface: &Surface,
        source: &[u8],
        budget: &Budget,
        tenant: TenantId,
        metadata: RequestReportMetadata,
        metrics: &mut RequestMetrics,
    ) -> Result<Arc<BytecodeChunk>, Box<RequestReport>> {
        let cache_key = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(format!("{surface:?}").as_bytes());
            hasher.update(source);
            hasher.update(&serde_json::to_vec(&self.compile_policy).unwrap_or_default());
            if let Some(module_source) = surface.module_source() {
                hasher.update(&[1]);
                hasher.update(format!("{:p}", Arc::as_ptr(&module_source)).as_bytes());
                hasher.update(&module_source.epoch().to_le_bytes());
            } else {
                hasher.update(&[0]);
                hasher.update(&0_u64.to_le_bytes());
            }
            *hasher.finalize().as_bytes()
        };
        if let Some(verdict) = self.front_door_cache.get(&cache_key) {
            metrics.parse_ast_nodes = verdict.ast_nodes;
            metrics.type_arena_nodes = verdict.type_arena_nodes;
            if let Err(error) = enforce_front_door_limit(
                FrontDoorStage::Parse,
                FrontDoorLimit::ParseAstNodes,
                verdict.ast_nodes,
                self.front_door.max_parse_ast_nodes,
            ) {
                return Err(Box::new(request_report_error(
                    error, *metrics, metadata, tenant,
                )));
            }
            if let Err(error) = enforce_front_door_limit(
                FrontDoorStage::TypeCheck,
                FrontDoorLimit::ArenaNodes,
                verdict.type_arena_nodes,
                self.front_door.max_type_arena_nodes,
            ) {
                return Err(Box::new(request_report_error(
                    error, *metrics, metadata, tenant,
                )));
            }
            return match verdict.outcome {
                FrontDoorOutcome::TypeErrors(diagnostics) => Err(Box::new(request_report_error(
                    RequestError::TypeErrors(diagnostics),
                    *metrics,
                    metadata,
                    tenant,
                ))),
                FrontDoorOutcome::Chunk(chunk) => Ok(chunk),
            };
        }

        let shared_source: Arc<[u8]> = Arc::from(source);
        let started = Instant::now();
        let parse_source = Arc::clone(&shared_source);
        let parse_options = Options::default();
        let syntax_flags = SyntaxFlags::default();
        let ast_nodes = match run_front_door_stage(
            budget,
            "parse-budget",
            Arc::clone(&self.front_door_permits),
            move || {
                Ok(front_door_ast_node_count(
                    &parse_source,
                    parse_options,
                    syntax_flags,
                ))
            },
        )
        .await
        {
            Ok(ast_nodes) => ast_nodes,
            Err(error) => {
                metrics.parse_time = started.elapsed();
                return Err(Box::new(request_report_error(
                    error, *metrics, metadata, tenant,
                )));
            }
        };
        metrics.parse_time = started.elapsed();
        metrics.parse_ast_nodes = ast_nodes.unwrap_or(0);
        if let Some(used) = ast_nodes
            && let Err(error) = enforce_front_door_limit(
                FrontDoorStage::Parse,
                FrontDoorLimit::ParseAstNodes,
                used,
                self.front_door.max_parse_ast_nodes,
            )
        {
            return Err(Box::new(request_report_error(
                error, *metrics, metadata, tenant,
            )));
        }

        let started = Instant::now();
        let check_source = source.to_vec();
        let check_surface = surface.clone();
        let max_type_diagnostics = self.front_door.max_type_diagnostics;
        let check_parse_options = Options::default();
        let check_syntax_flags = SyntaxFlags::default();
        let check_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let checker_cancel = Arc::clone(&check_cancel);
        let (has_type_errors, diagnostics, type_arena_nodes) = match run_async_front_door_stage(
            budget,
            "type-check",
            Arc::clone(&self.front_door_permits),
            check_cancel,
            async move {
                let result = check_source_graph_for_surface(
                    &check_surface,
                    check_source,
                    check_parse_options,
                    check_syntax_flags,
                    max_type_diagnostics,
                    Some(checker_cancel),
                )
                .await;
                Ok::<_, RequestError>(result)
            },
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                metrics.check_time = started.elapsed();
                return Err(Box::new(request_report_error(
                    error, *metrics, metadata, tenant,
                )));
            }
        };
        metrics.check_time = started.elapsed();
        metrics.type_arena_nodes = type_arena_nodes;
        if let Err(error) = enforce_front_door_limit(
            FrontDoorStage::TypeCheck,
            FrontDoorLimit::ArenaNodes,
            type_arena_nodes,
            self.front_door.max_type_arena_nodes,
        ) {
            return Err(Box::new(request_report_error(
                error, *metrics, metadata, tenant,
            )));
        }
        if has_type_errors {
            self.front_door_cache.insert(
                cache_key,
                FrontDoorVerdict {
                    ast_nodes: metrics.parse_ast_nodes,
                    type_arena_nodes: metrics.type_arena_nodes,
                    outcome: FrontDoorOutcome::TypeErrors(diagnostics.clone()),
                },
            );
            return Err(Box::new(request_report_error(
                RequestError::TypeErrors(diagnostics),
                *metrics,
                metadata,
                tenant,
            )));
        }

        let started = Instant::now();
        let compile_source = Arc::clone(&shared_source);
        let compile_surface = surface.clone();
        let compile_policy = self.compile_policy.clone();
        let chunk = match run_front_door_stage(
            budget,
            "compile",
            Arc::clone(&self.front_door_permits),
            move || {
                compile_surface
                    .compile_with_options(&compile_source, &compile_policy)
                    .map_err(RequestError::Compile)
            },
        )
        .await
        {
            Ok(chunk) => chunk,
            Err(error) => {
                metrics.compile_time = started.elapsed();
                return Err(Box::new(request_report_error(
                    error, *metrics, metadata, tenant,
                )));
            }
        };
        metrics.compile_time = started.elapsed();
        let chunk = Arc::new(chunk);
        self.front_door_cache.insert(
            cache_key,
            FrontDoorVerdict {
                ast_nodes: metrics.parse_ast_nodes,
                type_arena_nodes: metrics.type_arena_nodes,
                outcome: FrontDoorOutcome::Chunk(Arc::clone(&chunk)),
            },
        );
        Ok(chunk)
    }

    fn enforce_compiled_limits(
        &self,
        chunk: &BytecodeChunk,
        metrics: &mut RequestMetrics,
        metadata: RequestReportMetadata,
        tenant: TenantId,
    ) -> Result<(), Box<RequestReport>> {
        let bytecode_metrics = match compiled_bytecode_metrics(chunk) {
            Ok(metrics) => metrics,
            Err(error) => {
                return Err(Box::new(request_report_error(
                    error, *metrics, metadata, tenant,
                )));
            }
        };
        metrics.compiled_instructions = bytecode_metrics.instructions;
        metrics.compiled_bytecode_bytes = bytecode_metrics.encoded_bytes;
        if let Err(error) = enforce_front_door_limit(
            FrontDoorStage::Compile,
            FrontDoorLimit::CompiledInstructions,
            bytecode_metrics.instructions,
            self.front_door.max_compiled_instructions,
        ) {
            return Err(Box::new(request_report_error(
                error, *metrics, metadata, tenant,
            )));
        }
        if let Err(error) = enforce_front_door_limit(
            FrontDoorStage::Compile,
            FrontDoorLimit::CompiledBytecodeBytes,
            bytecode_metrics.encoded_bytes,
            self.front_door.max_compiled_bytecode_bytes,
        ) {
            return Err(Box::new(request_report_error(
                error, *metrics, metadata, tenant,
            )));
        }
        Ok(())
    }

    fn aggregate_reservation(&self, source_bytes: usize, budget: &Budget) -> TenantResourceTotals {
        let remaining = budget.deadline.saturating_duration_since(Instant::now());
        let mut reservation = TenantResourceTotals {
            requests: 1,
            source_bytes: u64::try_from(source_bytes).unwrap_or(u64::MAX),
            ..TenantResourceTotals::default()
        };
        if let Some(cap) = self.aggregate_limits.max_front_door_time {
            reservation.front_door_time = remaining.min(cap);
        }
        if let Some(cap) = self.aggregate_limits.max_run_time {
            reservation.run_time = remaining.min(cap);
        }
        if let Some(cap) = self.aggregate_limits.max_gas_spent {
            reservation.gas_spent = self.base_limits.gas.unwrap_or(cap).min(cap);
        }
        if let Some(cap) = self.aggregate_limits.max_charged_bytes {
            let bytecode =
                u64::try_from(self.front_door.max_compiled_bytecode_bytes).unwrap_or(u64::MAX);
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
        report: RequestReport,
        reservation: TenantResourceReservation,
    ) -> RequestReport {
        if should_record_report_resources(&report) {
            reservation.settle(&report.metrics);
        }
        report
    }

    pub(crate) fn runtime_compiler_for_surface(
        &self,
        surface: &Surface,
    ) -> Arc<dyn RuntimeCompiler> {
        #[cfg(any())]
        if let Some(compiler) = &self.runtime_compiler_override {
            return Arc::clone(compiler);
        }
        Arc::new(RunnerRuntimeCompiler {
            surface: surface.clone(),
            max_source_bytes: self.max_source_bytes,
            compile_policy: self.compile_policy.clone(),
            front_door: self.front_door,
        })
    }
}

fn should_record_report_resources(report: &RequestReport) -> bool {
    !(report.metrics.parse_time == Duration::ZERO
        && report.metrics.check_time == Duration::ZERO
        && report.metrics.compile_time == Duration::ZERO
        && report.metrics.vm_build_time == Duration::ZERO
        && report.metrics.sandbox_time == Duration::ZERO
        && report.metrics.load_time == Duration::ZERO
        && report.metrics.run_time == Duration::ZERO)
}

struct AdmittedRequest {
    metadata: RequestReportMetadata,
    metrics: RequestMetrics,
    _ingress: IngressGuard,
    reservation: TenantResourceReservation,
}

struct LaneRequest {
    surface: Surface,
    ambient: Ambient,
    limits: Limits,
    runtime_compiler: Arc<dyn RuntimeCompiler>,
    chunk: Arc<BytecodeChunk>,
    deadline: Instant,
    exec_cancel: Cancel,
    metrics: RequestMetrics,
}

struct LaneRequestResult {
    metrics: RequestMetrics,
    outcome: Result<Vec<ResultValue>, RequestError>,
}

async fn run_request_vm_on_lane(request: LaneRequest) -> LaneRequestResult {
    let LaneRequest {
        surface,
        ambient,
        limits,
        runtime_compiler,
        chunk,
        deadline,
        exec_cancel,
        mut metrics,
    } = request;

    if exec_cancel.is_cancelled() {
        return LaneRequestResult {
            metrics,
            outcome: Err(cancelled_or_deadline(deadline)),
        };
    }

    let started = Instant::now();
    let builder = surface
        .vm_builder(&VmConfig::untrusted(ambient, limits))
        .runtime_compiler(runtime_compiler);
    // The runner build validates that every lane submission carries ambient,
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
        Err(error) => panic!("runner sets ambient, limits, and runtime capabilities: {error}"),
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
        Ok(values) => Ok(values.into_iter().map(ResultValue::from).collect()),
        Err(error) => Err(map_exec_error(error, deadline)),
    };
    LaneRequestResult { metrics, outcome }
}

fn cancelled_or_deadline(deadline: Instant) -> RequestError {
    if Instant::now() >= deadline {
        RequestError::DeadlineExceeded
    } else {
        RequestError::Cancelled
    }
}

fn record_vm_metrics(metrics: &mut RequestMetrics, vm: &Vm) {
    metrics.heap_bytes = vm.heap_used_bytes();
    metrics.peak_heap_bytes = vm.peak_heap_bytes();
    metrics.gc_cycles = vm.gc_cycles();
    metrics.gas_spent = vm.gas_spent();
    metrics.vm_execution_count = vm.execution_count();
}

pub fn map_unwind_error(
    kind: RuntimeErrorKind,
    rendered: ResultValue,
    deadline: Instant,
) -> RequestError {
    match kind {
        // The deadline->cancel bridge stops a CPU loop by cancelling, so a
        // cancellation seen after the deadline has passed is attributed to the
        // deadline; an earlier one is a genuine caller cancellation.
        RuntimeErrorKind::Cancelled if Instant::now() >= deadline => RequestError::DeadlineExceeded,
        RuntimeErrorKind::Cancelled => RequestError::Cancelled,
        RuntimeErrorKind::Deadline => RequestError::DeadlineExceeded,
        RuntimeErrorKind::Memory => RequestError::OutOfMemory(rendered),
        RuntimeErrorKind::PanicPoison => RequestError::PanicPoison(rendered),
        RuntimeErrorKind::HandlerFailure => RequestError::HandlerFailure(rendered),
        RuntimeErrorKind::UnresolvedRequire => RequestError::Runtime(rendered),
        RuntimeErrorKind::Runtime => RequestError::Runtime(rendered),
    }
}

fn map_exec_error(error: ExecError, deadline: Instant) -> RequestError {
    match error {
        ExecError::Script(error) => map_unwind_error(
            error.kind(),
            ResultValue::from(error.value().clone()),
            deadline,
        ),
        ExecError::Cancelled if Instant::now() >= deadline => RequestError::DeadlineExceeded,
        ExecError::Cancelled => RequestError::Cancelled,
        ExecError::Deadline => RequestError::DeadlineExceeded,
        ExecError::PanicPoison => {
            RequestError::PanicPoison(ResultValue::String(b"VM is poisoned".to_vec()))
        }
        ExecError::Marshal { message } => RequestError::Runtime(ResultValue::String(
            format!("result marshal failed: {message}").into_bytes(),
        )),
    }
}

pub fn map_load_error(error: LoadError) -> RequestError {
    match error {
        LoadError::OutOfMemory => RequestError::OutOfMemory(ResultValue::String(
            b"out of memory loading bytecode".to_vec(),
        )),
        other => RequestError::Load(other),
    }
}

pub fn enforce_front_door_limit(
    stage: FrontDoorStage,
    limit: FrontDoorLimit,
    used: usize,
    cap: usize,
) -> Result<(), RequestError> {
    if used > cap {
        return Err(RequestError::FrontDoorLimitExceeded {
            stage,
            limit,
            used,
            cap,
        });
    }
    Ok(())
}

pub fn front_door_ast_node_count(
    source: &[u8],
    options: Options,
    syntax_flags: SyntaxFlags,
) -> Option<usize> {
    let parsed = parse_file_bytes_with(source, options, syntax_flags);
    parsed.root.as_ref().map(ast_node_count)
}

fn ast_node_count(root: &Stat) -> usize {
    #[derive(Default)]
    struct Counter {
        nodes: usize,
    }

    impl Visitor<'_> for Counter {
        fn visit_stat(&mut self, _path: &NodePath, _stat: &Stat) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }

        fn visit_local(&mut self, _path: &NodePath, _local: &Local) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }

        fn visit_expr(&mut self, _path: &NodePath, _expr: &Expr) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }

        fn visit_type(&mut self, _path: &NodePath, _luau_type: &Type) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }

        fn visit_type_pack(&mut self, _path: &NodePath, _type_pack: &TypePack) -> WalkControl {
            self.nodes += 1;
            WalkControl::Continue
        }
    }

    let mut counter = Counter::default();
    walk_stat(root, &mut counter);
    counter.nodes
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompiledBytecodeMetrics {
    instructions: usize,
    encoded_bytes: usize,
}

struct RunnerRuntimeCompiler {
    surface: Surface,
    max_source_bytes: usize,
    compile_policy: CompileOptions,
    front_door: FrontDoorLimits,
}

struct RuntimeCompileCancellation {
    flag: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    watcher: Option<thread::JoinHandle<()>>,
}

impl RuntimeCompileCancellation {
    fn new(cancel: Option<Cancel>) -> Self {
        let flag = Arc::new(AtomicBool::new(
            cancel.as_ref().is_some_and(Cancel::is_cancelled),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = cancel.and_then(|cancel| {
            let flag = Arc::clone(&flag);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("ruau-runtime-compile-cancel".to_owned())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if cancel.is_cancelled() {
                            flag.store(true, Ordering::Relaxed);
                            return;
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                })
                .ok()
        });
        Self {
            flag,
            stop,
            watcher,
        }
    }

    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    fn check_cancelled(&self) -> Result<(), Vec<u8>> {
        if self.flag.load(Ordering::Relaxed) {
            return Err(runtime_compilation_cancelled());
        }
        Ok(())
    }
}

impl Drop for RuntimeCompileCancellation {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(watcher) = self.watcher.take() {
            drop(watcher.join());
        }
    }
}

impl RuntimeCompiler for RunnerRuntimeCompiler {
    fn compile(
        &self,
        source: &[u8],
        context: RuntimeCompileContext,
    ) -> Result<BytecodeChunk, Vec<u8>> {
        let cancellation = RuntimeCompileCancellation::new(context.cancel.clone());
        cancellation.check_cancelled()?;
        let limits = context.limits;
        enforce_runner_runtime_compile_limit("source byte", source.len(), limits.max_source_bytes)?;
        if source.len() > self.max_source_bytes {
            return Err(format!(
                "runtime compilation source byte limit exceeded: {} > {}",
                source.len(),
                self.max_source_bytes
            )
            .into_bytes());
        }
        cancellation.check_cancelled()?;

        if let Some(ast_nodes) =
            front_door_ast_node_count(source, Options::default(), SyntaxFlags::default())
            && ast_nodes > self.front_door.max_parse_ast_nodes
        {
            return Err(format!(
                "runtime compilation parse AST node limit exceeded: {} > {}",
                ast_nodes, self.front_door.max_parse_ast_nodes
            )
            .into_bytes());
        }
        cancellation.check_cancelled()?;

        let check = if context.module_id.is_some() || std::str::from_utf8(source).is_ok() {
            check_runtime_source_ready(
                &self.surface,
                source,
                context.module_id,
                Options::default(),
                SyntaxFlags::default(),
                self.front_door.max_type_diagnostics,
                Some(cancellation.flag()),
            )
        } else {
            check_sourceless_source_bytes(
                &self.surface,
                source,
                Options::default(),
                SyntaxFlags::default(),
                self.front_door.max_type_diagnostics,
                Some(cancellation.flag()),
            )
        };
        cancellation.check_cancelled()?;
        if check.type_arena_nodes > self.front_door.max_type_arena_nodes {
            return Err(format!(
                "runtime compilation type arena node limit exceeded: {} > {}",
                check.type_arena_nodes, self.front_door.max_type_arena_nodes
            )
            .into_bytes());
        }
        if check.has_issues {
            return Err(format!(
                "runtime compilation type check failed: {:?}",
                check.diagnostics
            )
            .into_bytes());
        }
        cancellation.check_cancelled()?;

        let chunk = match self
            .surface
            .runtime_capabilities()
            .compile_source_with_cancel(source, &self.compile_policy, Some(cancellation.flag()))
        {
            Ok(BytecodeChunk::Error { message }) => return Err(message),
            Ok(valid @ BytecodeChunk::Valid { .. }) => valid,
            Err(error) if error.kind() == CompileErrorKind::Cancelled => {
                return Err(runtime_compilation_cancelled());
            }
            Err(error) => return Err(error.to_string().into_bytes()),
        };
        cancellation.check_cancelled()?;
        let metrics = match compiled_bytecode_metrics(&chunk) {
            Ok(metrics) => metrics,
            Err(error) => {
                return Err(format!("runtime compilation product failed: {error:?}").into_bytes());
            }
        };
        enforce_runner_runtime_compile_limit(
            "compiled instruction",
            metrics.instructions,
            limits
                .max_compiled_instructions
                .min(self.front_door.max_compiled_instructions),
        )?;
        enforce_runner_runtime_compile_limit(
            "compiled bytecode byte",
            metrics.encoded_bytes,
            limits
                .max_compiled_bytecode_bytes
                .min(self.front_door.max_compiled_bytecode_bytes),
        )?;
        Ok(chunk)
    }
}

fn runtime_compilation_cancelled() -> Vec<u8> {
    b"runtime compilation cancelled".to_vec()
}

fn enforce_runner_runtime_compile_limit(
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

pub fn compiled_bytecode_metrics(
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
        RequestError::Runtime(ResultValue::String(
            format!("source front-door compile product failed to encode: {error}").into_bytes(),
        ))
    })?;
    Ok(CompiledBytecodeMetrics {
        instructions,
        encoded_bytes: encoded_bytes.len(),
    })
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("surface", &self.surface)
            .field("ambient", &self.ambient)
            .field("features", &self.features)
            .field("max_source_bytes", &self.max_source_bytes)
            .field("front_door", &self.front_door)
            .field("ingress", &self.ingress.limits)
            .field("aggregate_limits", &self.aggregate_limits)
            .field("lane_pool", &self.lane_pool.metrics())
            .finish_non_exhaustive()
    }
}
pub fn check_front_door_budget(budget: &Budget) -> Result<(), RequestError> {
    if budget.cancel.is_cancelled() {
        return Err(RequestError::Cancelled);
    }
    if Instant::now() >= budget.deadline {
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

pub async fn run_front_door_stage<T>(
    budget: &Budget,
    stage: &'static str,
    permits: Arc<tokio::sync::Semaphore>,
    work: impl FnOnce() -> Result<T, RequestError> + Send + 'static,
) -> Result<T, RequestError>
where
    T: Send + 'static,
{
    check_front_door_budget(budget)?;
    let deadline = tokio::time::sleep_until(tokio::time::Instant::from_std(budget.deadline));
    tokio::pin!(deadline);
    // Bounded admission: wait for a front-door CPU slot, still honoring
    // cancellation and the deadline while queued.
    let permit = tokio::select! {
        biased;
        () = budget.cancel.cancelled() => return Err(RequestError::Cancelled),
        () = &mut deadline => return Err(RequestError::DeadlineExceeded),
        permit = permits.acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(_) => {
                return Err(RequestError::Runtime(ResultValue::String(
                    format!("source front-door {stage} pool is closed").into_bytes(),
                )));
            }
        },
    };
    let handle = tokio::task::spawn_blocking(work);
    let result = tokio::select! {
        biased;
        () = budget.cancel.cancelled() => Err(RequestError::Cancelled),
        () = &mut deadline => Err(RequestError::DeadlineExceeded),
        joined = handle => match joined {
            Ok(result) => result,
            Err(error) if error.is_panic() => Err(RequestError::PanicPoison(ResultValue::String(
                format!("source front-door {stage} task panicked").into_bytes(),
            ))),
            Err(_) => Err(RequestError::Runtime(ResultValue::String(
                format!("source front-door {stage} task was cancelled").into_bytes(),
            ))),
        },
    };
    drop(permit);
    result
}

enum AsyncFrontDoorStageResult<T> {
    Finished(Result<T, RequestError>),
    Panic,
}

pub struct FrontDoorAsyncRuntimePool {
    runtimes: Mutex<Vec<tokio::runtime::Runtime>>,
}

struct FrontDoorAsyncRuntimeLease {
    pool: Arc<FrontDoorAsyncRuntimePool>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl FrontDoorAsyncRuntimePool {
    fn new(size: usize) -> Result<Self, String> {
        let mut runtimes = Vec::with_capacity(size);
        for _ in 0..size {
            runtimes.push(build_front_door_current_thread_runtime()?);
        }
        Ok(Self {
            runtimes: Mutex::new(runtimes),
        })
    }

    fn lease(self: &Arc<Self>) -> Result<FrontDoorAsyncRuntimeLease, String> {
        let runtime = self
            .runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .map_or_else(build_front_door_current_thread_runtime, Ok)?;
        Ok(FrontDoorAsyncRuntimeLease {
            pool: Arc::clone(self),
            runtime: Some(runtime),
        })
    }
}

impl FrontDoorAsyncRuntimeLease {
    fn block_on<F: std::future::Future>(&mut self, future: F) -> F::Output {
        self.runtime
            .as_mut()
            .expect("front-door runtime lease always owns a runtime")
            .block_on(future)
    }
}

impl Drop for FrontDoorAsyncRuntimeLease {
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

fn build_front_door_current_thread_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

pub fn front_door_async_runtime() -> Result<Arc<FrontDoorAsyncRuntimePool>, String> {
    FRONT_DOOR_ASYNC_RUNTIME
        .get_or_init(|| {
            FrontDoorAsyncRuntimePool::new(default_type_check_concurrency()).map(Arc::new)
        })
        .clone()
}

pub async fn run_async_front_door_stage<T, F>(
    budget: &Budget,
    stage: &'static str,
    permits: Arc<tokio::sync::Semaphore>,
    cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    work: F,
) -> Result<T, RequestError>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, RequestError>> + Send + 'static,
{
    check_front_door_budget(budget)?;
    let deadline = tokio::time::sleep_until(tokio::time::Instant::from_std(budget.deadline));
    tokio::pin!(deadline);
    // Bounded admission: wait for a stage slot, still honoring cancellation
    // and the deadline while queued.
    let permit = tokio::select! {
        biased;
        () = budget.cancel.cancelled() => return Err(RequestError::Cancelled),
        () = &mut deadline => return Err(RequestError::DeadlineExceeded),
        permit = permits.acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(_) => {
                return Err(RequestError::Runtime(ResultValue::String(
                    format!("source front-door {stage} pool is closed").into_bytes(),
                )));
            }
        },
    };
    let runtime = front_door_async_runtime().map_err(|error| {
        RequestError::Runtime(ResultValue::String(
            format!("source front-door {stage} async runtime failed: {error}").into_bytes(),
        ))
    })?;
    let mut runtime = runtime.lease().map_err(|error| {
        RequestError::Runtime(ResultValue::String(
            format!("source front-door {stage} async runtime failed: {error}").into_bytes(),
        ))
    })?;
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
            Ok(result) => AsyncFrontDoorStageResult::Finished(result),
            Err(_) => AsyncFrontDoorStageResult::Panic,
        }
    });

    tokio::select! {
        biased;
        () = budget.cancel.cancelled() => {
            // Stop the abandoned worker: the flag halts CPU-bound loops (the
            // constraint solver polls it) and dropping the sender wakes the
            // worker's select at its next await point.
            cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            drop(cancel_tx);
            Err(RequestError::Cancelled)
        }
        () = &mut deadline => {
            cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            drop(cancel_tx);
            Err(RequestError::DeadlineExceeded)
        }
        joined = handle => match joined {
            Ok(AsyncFrontDoorStageResult::Finished(result)) => result,
            Ok(AsyncFrontDoorStageResult::Panic) => Err(RequestError::PanicPoison(ResultValue::String(
                format!("source front-door {stage} task panicked").into_bytes(),
            ))),
            Err(_) => Err(RequestError::Runtime(ResultValue::String(
                format!("source front-door {stage} task was cancelled").into_bytes(),
            ))),
        },
    }
}
