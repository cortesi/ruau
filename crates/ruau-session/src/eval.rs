//! Retained source-eval host over a validated [`Surface`].

use std::{
    any::Any,
    error::Error as StdError,
    fmt,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ruau_bytecode::{BytecodeChunk, CompileError, CompileOptions};
use ruau_source::{ModuleId, ModuleName, Source, SourceMetadata};
use ruau_surface::{PrepareGraphError, PrepareOptions, PreparedGraph, Surface, VmConfig};
use ruau_vm::{
    Ambient, CallOptions, Cancel, Deadline, ExecError, Limits, MarshaledScriptError, ModuleBinding,
    NativeModule, RuntimeErrorKind, SinkQuota, StopReason, ValueSnapshot, VmBuildError,
    module::{
        Array as ModuleArray, Installer as ModuleBuilder, Table as ModuleTable,
        Value as ModuleValue,
    },
    serde::{integer_module_value, json_null_module_value},
};
use serde_json::{Map, Number, Value};

use crate::{BlockingRuntime, BlockingRuntimeError};

const DEFAULT_CHUNK_NAME: &str = "eval.luau";
const DEFAULT_PRINT_MAX_BYTES: usize = 64 * 1024;
const DEFAULT_PRINT_MAX_CALLS: usize = 1024;
/// Default wall-clock timeout for retained evaluation of untrusted source.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);
/// Default gas budget for retained evaluation of untrusted source.
pub const DEFAULT_GAS: u64 = 50_000_000;
/// Default in-VM memory cap for retained evaluation of untrusted source.
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 32 * 1024 * 1024;

/// Retained source evaluator for ordinary embedding hosts.
///
/// No Tokio setup is required: the blocking entry points own a lazily created
/// runtime, and the async entry points run on whatever runtime drives them.
pub struct Evaluator {
    surface: Surface,
    compile_policy: CompileOptions,
    blocking_runtime: BlockingRuntime,
    next_seed: AtomicU64,
}

impl fmt::Debug for Evaluator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Evaluator")
            .field("surface", &self.surface)
            .field("compile_policy", &self.compile_policy)
            .field("next_seed", &self.next_seed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Evaluator {
    /// Builds a host over a validated surface.
    #[must_use]
    pub fn new(surface: Surface) -> Self {
        Self {
            surface,
            compile_policy: CompileOptions::default(),
            blocking_runtime: BlockingRuntime::new("ruau-session-blocking-eval"),
            next_seed: AtomicU64::new(1),
        }
    }

    /// Replaces the compile policy used for future evaluations.
    #[must_use]
    pub fn with_compile_policy(mut self, policy: CompileOptions) -> Self {
        self.compile_policy = policy;
        self
    }

    /// Returns this host's retained surface.
    #[must_use]
    pub const fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Returns this host's compile policy.
    #[must_use]
    pub const fn compile_policy(&self) -> &CompileOptions {
        &self.compile_policy
    }

    /// Evaluates source on the retained host, blocking the calling thread.
    ///
    /// The evaluation is driven on a private single-worker Tokio runtime that
    /// this host lazily creates on first use and caches for its lifetime;
    /// callers need no Tokio setup of their own. Concurrent blocking
    /// evaluations through one shared host are supported.
    ///
    /// Ambient Tokio runtime contexts are handled without panicking:
    /// - Outside any runtime context, the future is driven directly on the
    ///   host's runtime.
    /// - Inside a multi-thread runtime context (an async task, a
    ///   [`tokio::task::spawn_blocking`] closure, or a
    ///   [`tokio::runtime::Handle::enter`] scope), the call blocks legally via
    ///   [`tokio::task::block_in_place`]; note this stalls the calling worker
    ///   thread for the whole evaluation, so prefer [`Self::eval`] in async
    ///   code.
    /// - Inside a current-thread runtime context or a
    ///   [`tokio::task::LocalSet`], blocking is impossible; the call returns
    ///   an [`ErrorKind::AsyncContext`] error instead of panicking. Use
    ///   [`Self::eval`] there.
    ///
    /// # Errors
    /// Returns [`Error`] for calls from contexts that cannot block, runtime
    /// construction, argument conversion, VM construction, compilation,
    /// loading, runtime, cancellation, timeout, and JSON result conversion
    /// failures.
    pub fn eval_blocking(&self, source: &str, options: Options) -> Result<Output, Error> {
        let chunk_name = options.chunk_name.clone();
        self.drive_blocking(&chunk_name, source, self.eval(source, options))
    }

    /// Checks and evaluates source on the retained host, blocking the calling
    /// thread.
    ///
    /// Shares [`Self::eval_blocking`]'s runtime semantics: the future is
    /// driven on this host's lazily created, cached single-worker Tokio
    /// runtime; ambient multi-thread runtime contexts block via
    /// [`tokio::task::block_in_place`]; and contexts that cannot block (a
    /// current-thread runtime context or a [`tokio::task::LocalSet`]) return
    /// an [`ErrorKind::AsyncContext`] error instead of panicking. Use
    /// [`Self::eval_checked`] in async code.
    ///
    /// # Errors
    /// Returns [`Error`] for calls from contexts that cannot block, runtime
    /// construction, static checking, argument conversion, VM construction,
    /// compilation, loading, runtime, cancellation, timeout, and JSON result
    /// conversion failures.
    pub fn eval_checked_blocking(&self, source: &str, options: Options) -> Result<Output, Error> {
        let chunk_name = options.chunk_name.clone();
        self.drive_blocking(&chunk_name, source, self.eval_checked(source, options))
    }

    /// Drives an evaluation future to completion on the host's blocking
    /// runtime, adapting to the calling thread's Tokio context.
    fn drive_blocking<F>(&self, chunk_name: &str, source: &str, future: F) -> Result<Output, Error>
    where
        F: Future<Output = Result<Output, Error>>,
    {
        self.blocking_runtime
            .block_on(future)
            .map_err(|error| Error::from_blocking(chunk_name, source, error))?
    }

    /// Evaluates source on the async VM driver.
    ///
    /// # Errors
    /// Returns [`Error`] for argument conversion, VM construction,
    /// compilation, loading, runtime, cancellation, timeout, and JSON result
    /// conversion failures.
    pub async fn eval(&self, source: &str, options: Options) -> Result<Output, Error> {
        let started = Instant::now();
        let source_text = source.to_owned();
        let script = options.source(source_text.clone());
        let chunk_name = script.display_name().to_owned();
        let compiled = self.compile_eval_script(script, source_text, chunk_name)?;
        self.exec_compiled(compiled, options, started, None).await
    }

    /// Checks source, then evaluates it on the async VM driver.
    ///
    /// When the retained surface has a module source, this checks the evaluated
    /// source as a synthetic graph root so static diagnostics from dependencies
    /// are reported before runtime execution. [`Self::eval`] remains the
    /// runtime-only path for callers that intentionally skip static checking.
    ///
    /// # Errors
    /// Returns [`Error`] for static checking, argument conversion, VM
    /// construction, compilation, loading, runtime, cancellation, timeout, and
    /// JSON result conversion failures.
    pub async fn eval_checked(&self, source: &str, options: Options) -> Result<Output, Error> {
        let started = Instant::now();
        let source_text = source.to_owned();
        let script = options.source(source_text.clone());
        let chunk_name = script.display_name().to_owned();

        let check_start = Instant::now();
        let graph = self
            .surface
            .check_graph(
                ruau_surface::GraphRoot::overlay(&script),
                ruau_surface::GraphCheckOptions::default(),
            )
            .await
            .map_err(|error| {
                self.graph_prepare_error(
                    &chunk_name,
                    &source_text,
                    PrepareGraphError::GraphCheck {
                        source: Box::new(script.clone()),
                        error,
                    },
                )
            })?;
        self.reject_graph_errors(&script, &chunk_name, &source_text, &graph)?;
        let check = check_start.elapsed();

        let compile_start = Instant::now();
        let prepared = self
            .surface
            .prepare_checked_graph(
                graph,
                PrepareOptions::new().with_compile_options(self.compile_policy.clone()),
            )
            .map_err(|error| self.graph_prepare_error(&chunk_name, &source_text, error))?;
        let compiled = CompiledEval {
            artifact: EvalArtifact::Graph(Box::new(prepared)),
            source_text,
            chunk_name,
            compile: compile_start.elapsed(),
        };
        self.exec_compiled(compiled, options, started, Some(check))
            .await
    }

    fn reject_graph_errors(
        &self,
        script: &Source,
        chunk_name: &str,
        source_text: &str,
        graph: &ruau_typecheck::CheckedGraph,
    ) -> Result<(), Error> {
        if !graph.has_errors() {
            return Ok(());
        }
        let diagnostics = graph.diagnostics();
        let root = source_root_name(script);
        let root_location = diagnostics
            .entries()
            .iter()
            .find(|entry| entry.module == root)
            .map(|entry| {
                let begin = entry.diagnostic.primary_location.begin;
                (begin.line, begin.column)
            });
        let (line, column) = diagnostic_line_column(root_location);
        Err(Error::new(
            ErrorKind::Check,
            chunk_name,
            source_text,
            line,
            column,
            diagnostics.render(),
        ))
    }

    fn graph_prepare_error(
        &self,
        chunk_name: &str,
        source_text: &str,
        error: PrepareGraphError,
    ) -> Error {
        match error {
            PrepareGraphError::Compile { error, .. } => {
                Error::from_compile(chunk_name, source_text, &error)
            }
            other => Error::new(
                ErrorKind::Check,
                chunk_name,
                source_text,
                None,
                None,
                other.to_string(),
            ),
        }
    }

    fn compile_eval_script(
        &self,
        script: Source,
        source_text: String,
        chunk_name: String,
    ) -> Result<CompiledEval, Error> {
        let compile_start = Instant::now();
        let chunk = self
            .surface
            .compile(&script, &self.compile_policy)
            .map_err(|error| Error::from_compile(&chunk_name, &source_text, &error))?;
        Ok(CompiledEval {
            artifact: EvalArtifact::Source { script, chunk },
            source_text,
            chunk_name,
            compile: compile_start.elapsed(),
        })
    }

    async fn exec_compiled(
        &self,
        compiled: CompiledEval,
        options: Options,
        started: Instant,
        check: Option<Duration>,
    ) -> Result<Output, Error> {
        let CompiledEval {
            artifact,
            source_text,
            chunk_name,
            compile,
        } = compiled;
        let args = json_to_module_value(&options.args).map_err(|message| {
            Error::new(
                ErrorKind::Args,
                &chunk_name,
                &source_text,
                None,
                None,
                message,
            )
        })?;

        let mut vm = self
            .surface
            .vm_builder(&VmConfig::untrusted(
                self.next_ambient(),
                options.limits.clone(),
            ))
            .module(Arc::new(GlobalValueModule::new("args", args)))
            .build()
            .map_err(|error| Error::from_build(&chunk_name, &source_text, error))?;
        let runtime_compiler = match &artifact {
            EvalArtifact::Source { .. } => None,
            EvalArtifact::Graph(prepared) => prepared.runtime_compiler(),
        };
        let module = match &artifact {
            EvalArtifact::Source { script, chunk } => {
                let load_name = script.load_name();
                vm.load_named_module(chunk, script.id().clone(), &load_name)
                    .map_err(|error| {
                        Error::new(
                            ErrorKind::Load,
                            &chunk_name,
                            &source_text,
                            None,
                            None,
                            format!("script load failed: {error}"),
                        )
                    })?
            }
            EvalArtifact::Graph(prepared) => prepared.load(&mut vm).map_err(|error| {
                Error::new(
                    ErrorKind::Load,
                    &chunk_name,
                    &source_text,
                    None,
                    None,
                    error.to_string(),
                )
            })?,
        };

        let prints = Arc::new(Mutex::new(Vec::<String>::new()));
        let limits = limits_for_eval(options.timeout, options.cancel.clone());
        let mut call_options = CallOptions::new()
            .limits(limits)
            .print_sink_with_quota(print_sink(Arc::clone(&prints)), options.print_quota);
        for value in options.app_data {
            call_options = call_options.app_data_erased(value);
        }
        if let Some(compiler) = runtime_compiler {
            call_options = call_options.runtime_compiler(compiler);
        }

        let execute_start = Instant::now();
        let values = vm
            .exec_async(&module, call_options)
            .await
            .map_err(|error| {
                Error::from_exec(&chunk_name, &source_text, options.timeout, &error)
            })?;
        let execute = execute_start.elapsed();
        let value = eval_json_value(&values).map_err(|message| {
            Error::new(
                ErrorKind::Marshal,
                &chunk_name,
                &source_text,
                None,
                None,
                message,
            )
        })?;
        let prints = match Arc::try_unwrap(prints) {
            Ok(prints) => prints.into_inner().unwrap_or_default(),
            Err(prints) => prints
                .lock()
                .map(|prints| prints.clone())
                .unwrap_or_default(),
        };

        Ok(Output {
            value,
            prints,
            timing: Timing {
                check,
                compile,
                execute,
                total: started.elapsed(),
            },
        })
    }

    fn next_ambient(&self) -> Ambient {
        let sequence = self.next_seed.fetch_add(1, Ordering::Relaxed);
        let time_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos() as u64);
        Ambient::production(time_seed ^ sequence.rotate_left(17))
    }
}

struct CompiledEval {
    artifact: EvalArtifact,
    source_text: String,
    chunk_name: String,
    compile: Duration,
}

enum EvalArtifact {
    Source {
        script: Source,
        chunk: BytecodeChunk,
    },
    Graph(Box<PreparedGraph>),
}

/// Per-evaluation controls.
///
/// The default is the untrusted-source posture, enforced on four axes:
/// execution is wall-clock bounded by [`DEFAULT_TIMEOUT`], work is gas-metered
/// by [`DEFAULT_GAS`], in-VM memory (with its derived string, buffer, table,
/// pack, and runtime-compile caps) is bounded by the
/// [`Limits::production`] profile over [`DEFAULT_MAX_MEMORY_BYTES`], and print
/// output is quota-limited. Use [`Options::trusted`],
/// [`Options::without_timeout`], or [`Options::limits`] with wider ceilings
/// only for source whose resource use is controlled by the embedding host.
pub struct Options {
    /// Chunk name used for loading, traceback frames, and errors.
    chunk_name: String,
    /// Optional module requester identity used to resolve relative dependencies.
    ///
    /// When absent, the chunk name is also the module identity. This field does not affect the
    /// human-facing chunk name or diagnostic metadata.
    requester: Option<ModuleId>,
    /// Wall-clock timeout. When set, the host installs both a wall deadline and
    /// a cancellation watchdog. Defaults to [`DEFAULT_TIMEOUT`].
    timeout: Option<Duration>,
    /// External cancellation signal for this evaluation.
    cancel: Option<Cancel>,
    /// VM resource ceilings for this evaluation. Defaults to the bounded
    /// [`Limits::production`] profile over [`DEFAULT_GAS`] and
    /// [`DEFAULT_MAX_MEMORY_BYTES`].
    limits: Limits,
    /// JSON-shaped global installed as `args` before sandboxing.
    args: Value,
    app_data: Vec<Box<dyn Any + Send + Sync>>,
    /// Per-evaluation print quota.
    print_quota: SinkQuota,
}

impl Options {
    /// Builds explicitly trusted-source controls: no wall-clock timeout and
    /// unmetered VM limits.
    #[must_use]
    pub fn trusted() -> Self {
        Self::default()
            .without_timeout()
            .limits(Limits::unlimited())
    }

    /// Sets the chunk name used in runtime tracebacks and diagnostics.
    #[must_use]
    pub fn chunk_name(mut self, name: impl Into<String>) -> Self {
        self.chunk_name = name.into();
        self
    }

    /// Sets the module requester identity used to resolve relative dependencies.
    ///
    /// The chunk name remains the human-facing diagnostic and traceback name.
    #[must_use]
    pub fn requester(mut self, requester: impl Into<ModuleId>) -> Self {
        self.requester = Some(requester.into());
        self
    }

    /// Sets a wall-clock timeout for this evaluation.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Disables the default wall-clock timeout.
    ///
    /// Gas and memory metering from [`Options::limits`] still apply. Prefer
    /// this only when the source is trusted or independently bounded by the
    /// host.
    #[must_use]
    pub fn without_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// Sets the VM resource ceilings for this evaluation.
    ///
    /// This replaces the default bounded profile whole; passing
    /// [`Limits::unlimited`] removes gas and memory metering.
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets an external cancellation signal for this evaluation.
    #[must_use]
    pub fn cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Sets the JSON document installed as the `args` global.
    ///
    /// Explicit `null` values use the `json.null` sentinel, while arrays carry
    /// the protected marker that preserves empty-array identity.
    #[must_use]
    pub fn args(mut self, args: Value) -> Self {
        self.args = args;
        self
    }

    /// Adds typed app data visible through `Scope::app_data::<T>` during this
    /// evaluation.
    #[must_use]
    pub fn app_data<T: Any + Send + Sync>(mut self, value: T) -> Self {
        self.app_data.push(Box::new(value));
        self
    }

    /// Sets the print quota for this evaluation.
    #[must_use]
    pub fn print_quota(mut self, quota: SinkQuota) -> Self {
        self.print_quota = quota;
        self
    }

    fn source(&self, source: String) -> Source {
        let id = self
            .requester
            .clone()
            .unwrap_or_else(|| ModuleId::new(self.chunk_name.clone()));
        Source::text(id, source).with_metadata(SourceMetadata::new(self.chunk_name.clone()))
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            chunk_name: DEFAULT_CHUNK_NAME.to_owned(),
            requester: None,
            timeout: Some(DEFAULT_TIMEOUT),
            cancel: None,
            limits: Limits::metered(DEFAULT_GAS, DEFAULT_MAX_MEMORY_BYTES),
            args: Value::Object(Map::new()),
            app_data: Vec::new(),
            print_quota: SinkQuota {
                max_bytes: Some(DEFAULT_PRINT_MAX_BYTES),
                max_calls: Some(DEFAULT_PRINT_MAX_CALLS),
            },
        }
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Options")
            .field("chunk_name", &self.chunk_name)
            .field("requester", &self.requester)
            .field("timeout", &self.timeout)
            .field("cancel", &self.cancel)
            .field("limits", &self.limits)
            .field("args", &self.args)
            .field("app_data_len", &self.app_data.len())
            .field("print_quota", &self.print_quota)
            .finish()
    }
}

/// Successful evaluation output.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// JSON return value: `None` for no returns, one JSON value for one return,
    /// or a JSON array for multiple returns. Exactly-integral finite numbers
    /// encode as JSON integers.
    pub value: Option<Value>,
    /// Captured `print` output lines.
    pub prints: Vec<String>,
    /// Check, compile, execute, and total wall timings.
    pub timing: Timing,
}

/// Evaluation timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    /// Static check duration: `Some` when the evaluation ran the checked path
    /// ([`Evaluator::eval_checked`]), `None` when checking was skipped
    /// ([`Evaluator::eval`]).
    pub check: Option<Duration>,
    /// Source compile duration.
    pub compile: Duration,
    /// VM execution duration.
    pub execute: Duration,
    /// Total evaluation duration.
    pub total: Duration,
}

/// Evaluation failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// A blocking evaluation was called from a Tokio context that cannot
    /// block: a current-thread runtime context or a `LocalSet`.
    AsyncContext,
    /// JSON args could not be represented as module constants.
    Args,
    /// Static checking produced error-severity diagnostics.
    Check,
    /// VM (or blocking eval runtime) construction failed.
    Build,
    /// Source compilation failed.
    Compile,
    /// Bytecode loading failed.
    Load,
    /// Script raised a catchable runtime error.
    Runtime,
    /// Evaluation was cancelled.
    Cancelled,
    /// Evaluation exceeded its wall-clock timeout.
    ///
    /// This is the host-level name for the cap configured through
    /// [`Options::timeout`]; it corresponds to the VM's deadline stop reason
    /// and the runner's `RequestError::DeadlineExceeded` /
    /// `StopReason::Deadline` vocabulary.
    Timeout,
    /// The VM was poisoned by a panic.
    PanicPoison,
    /// Return-value marshaling or JSON conversion failed.
    Marshal,
}

/// Structured host-error categories that affect outer evaluation handling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StructuredErrorKind {
    /// The host operation failed while performing I/O.
    Io,
}

/// Structured evaluation error with source context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    /// Error category.
    pub kind: ErrorKind,
    /// Chunk name associated with the source.
    chunk_name: String,
    /// 1-based source line when known.
    pub line: Option<usize>,
    /// 1-based source column when known.
    pub column: Option<usize>,
    /// Human-readable message.
    pub message: String,
    /// Full source text.
    pub source: String,
    /// Classified `kind` when the script raised a recognized structured host error.
    pub structured_kind: Option<StructuredErrorKind>,
    /// Complete JSON object when the script raised a recognized structured error table.
    pub structured: Option<Box<Value>>,
}

impl Error {
    /// Returns the chunk name associated with the source.
    #[must_use]
    pub fn chunk_name(&self) -> &str {
        &self.chunk_name
    }

    fn new(
        kind: ErrorKind,
        chunk_name: &str,
        source: &str,
        line: Option<usize>,
        column: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            chunk_name: chunk_name.to_owned(),
            line,
            column,
            message: message.into(),
            source: source.to_owned(),
            structured_kind: None,
            structured: None,
        }
    }

    fn from_compile(chunk_name: &str, source: &str, error: &CompileError) -> Self {
        let (line, column) = error.location().map_or((None, None), |location| {
            (
                Some(location.begin.line as usize + 1),
                Some(location.begin.column as usize + 1),
            )
        });
        Self::new(
            ErrorKind::Compile,
            chunk_name,
            source,
            line,
            column,
            error.message().to_owned(),
        )
    }

    fn from_build(chunk_name: &str, source: &str, error: VmBuildError) -> Self {
        let message = match error {
            VmBuildError::Sandbox(error) => format!("VM sandboxing failed: {error}"),
            error => format!("VM build failed: {error}"),
        };
        Self::new(ErrorKind::Build, chunk_name, source, None, None, message)
    }

    fn from_exec(
        chunk_name: &str,
        source: &str,
        timeout: Option<Duration>,
        error: &ExecError,
    ) -> Self {
        match error {
            ExecError::Script(error) => {
                let (line, column) = runtime_location(error, chunk_name);
                let mut out = Self::new(
                    ErrorKind::Runtime,
                    chunk_name,
                    source,
                    line,
                    column,
                    runtime_message(error, chunk_name),
                );
                out.structured_kind = runtime_structured_kind(error);
                out.structured = error.structured_details().map(Box::new);
                out
            }
            ExecError::Stopped(StopReason::Cancelled) => Self::new(
                ErrorKind::Cancelled,
                chunk_name,
                source,
                None,
                None,
                timeout.map_or_else(|| "script cancelled".to_owned(), timeout_message),
            ),
            ExecError::Stopped(StopReason::Deadline) => Self::new(
                ErrorKind::Timeout,
                chunk_name,
                source,
                None,
                None,
                timeout.map_or_else(
                    || "script exceeded its deadline".to_owned(),
                    timeout_message,
                ),
            ),
            ExecError::PanicPoison => Self::new(
                ErrorKind::PanicPoison,
                chunk_name,
                source,
                None,
                None,
                "VM was poisoned by a panic",
            ),
            ExecError::Entry { message } => Self::new(
                ErrorKind::Runtime,
                chunk_name,
                source,
                None,
                None,
                message.clone(),
            ),
            ExecError::Marshal { message } => Self::new(
                ErrorKind::Marshal,
                chunk_name,
                source,
                None,
                None,
                message.clone(),
            ),
        }
    }

    fn from_blocking(chunk_name: &str, source: &str, error: BlockingRuntimeError) -> Self {
        match error {
            BlockingRuntimeError::AsyncContext => async_context_error(chunk_name, source),
            BlockingRuntimeError::Build(message) => {
                Self::new(ErrorKind::Build, chunk_name, source, None, None, message)
            }
        }
    }

    /// Pretty-prints this error with a source excerpt when a location is known.
    #[must_use]
    pub fn format_pretty(&self) -> String {
        let mut output = format!("{}: {}\n", self.chunk_name, self.message);
        let (Some(line), Some(column)) = (self.line, self.column) else {
            return output;
        };
        let lines = self.source.lines().collect::<Vec<_>>();
        if line == 0 || line > lines.len() {
            return output;
        }
        let width = lines.len().max(1).to_string().len();
        let text = lines[line - 1];
        output.push_str(&format!("> {line:>width$} | {text}\n"));
        let caret_column = column.max(1);
        output.push_str(&format!("  {:>width$} | ", ""));
        output.push_str(&" ".repeat(caret_column.saturating_sub(1)));
        output.push_str("^\n");
        output
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}

struct GlobalValueModule {
    name: String,
    value: ModuleValue,
}

impl GlobalValueModule {
    fn new(name: impl Into<String>, value: ModuleValue) -> Self {
        Self {
            name: name.into(),
            value,
        }
    }
}

impl NativeModule for GlobalValueModule {
    fn name(&self) -> &str {
        "ruau_session_globals"
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text("")
    }

    fn install(&self, builder: &mut dyn ModuleBuilder) {
        builder.constant(&self.name, ModuleBinding::Global, self.value.clone());
    }
}

fn json_to_module_value(value: &Value) -> Result<ModuleValue, String> {
    match value {
        Value::Null => Ok(json_null_module_value()),
        Value::Bool(value) => Ok(ModuleValue::Boolean(*value)),
        Value::Number(number) => json_number_to_module_value(number),
        Value::String(value) => Ok(ModuleValue::Bytes(value.as_bytes().to_vec())),
        Value::Array(values) => {
            let mut array = ModuleArray::new();
            for value in values {
                array = array.value(json_to_module_value(value)?);
            }
            Ok(ModuleValue::Array(array))
        }
        Value::Object(map) => {
            let mut table = ModuleTable::new();
            for (key, value) in map {
                table = table.entry(key.clone(), json_to_module_value(value)?);
            }
            Ok(ModuleValue::Table(table))
        }
    }
}

fn json_number_to_module_value(number: &Number) -> Result<ModuleValue, String> {
    if let Some(value) = number.as_i64() {
        return Ok(integer_module_value(value));
    }
    if let Some(value) = number.as_u64() {
        return i64::try_from(value)
            .map(integer_module_value)
            .map_err(|_| format!("JSON integer {value} exceeds Luau's i64 range"));
    }
    let value = number
        .as_f64()
        .ok_or_else(|| format!("JSON number {number} is not representable as f64"))?;
    Ok(ModuleValue::Number(value))
}

fn limits_for_eval(timeout: Option<Duration>, cancel: Option<Cancel>) -> Limits {
    let mut limits = Limits::unlimited();
    if let Some(timeout) = timeout {
        limits.deadline = Some(Deadline::Wall(
            Instant::now()
                .checked_add(timeout)
                .unwrap_or_else(Instant::now),
        ));
        let scoped_cancel = match cancel {
            Some(cancel) => cancel.child_after(timeout),
            None => Cancel::after(timeout),
        };
        limits.cancel = Some(scoped_cancel);
    } else {
        limits.cancel = cancel;
    }
    limits
}

fn print_sink(prints: Arc<Mutex<Vec<String>>>) -> ruau_vm::PrintSink {
    Box::new(move |line| {
        let message = String::from_utf8_lossy(line)
            .trim_end_matches('\n')
            .to_owned();
        if let Ok(mut prints) = prints.lock() {
            prints.push(message);
        }
    })
}

fn eval_json_value(values: &[ValueSnapshot]) -> Result<Option<Value>, String> {
    ruau_vm::serde::marshaled_return_values_to_json(values).map_err(|error| error.to_string())
}

fn source_root_name(source: &Source) -> ModuleName {
    ModuleName::from_id(source.id())
        .unwrap_or_else(|_| ModuleName::from(source.id().to_lossy_string()))
}

fn diagnostic_line_column(location: Option<(u32, u32)>) -> (Option<usize>, Option<usize>) {
    let Some((line, column)) = location else {
        return (None, None);
    };
    if line == u32::MAX || column == u32::MAX {
        return (None, None);
    }
    (
        Some(usize::try_from(line).expect("diagnostic line fits usize") + 1),
        Some(usize::try_from(column).expect("diagnostic column fits usize") + 1),
    )
}

fn runtime_location(
    error: &MarshaledScriptError,
    chunk_name: &str,
) -> (Option<usize>, Option<usize>) {
    let normalized = normalize_chunk_name(chunk_name);
    error
        .frames()
        .iter()
        .find(|frame| normalize_chunk_name(&frame.chunk_name) == normalized)
        .and_then(|frame| frame.line.map(|line| line as usize))
        .map_or((None, None), |line| (Some(line), Some(1)))
}

fn normalize_chunk_name(name: &str) -> &str {
    ruau_vm::ChunkNameRef::parse(name.as_bytes())
        .payload_str()
        .unwrap_or(name)
}

fn runtime_message(error: &MarshaledScriptError, chunk_name: &str) -> String {
    if matches!(
        error.kind(),
        RuntimeErrorKind::Cancelled | RuntimeErrorKind::Deadline
    ) {
        return "script timed out".to_owned();
    }
    match error.value() {
        ValueSnapshot::String(_) => strip_runtime_location(
            error.display_message(),
            chunk_name,
            runtime_location(error, chunk_name).0,
        ),
        _ => error.display_message(),
    }
}

/// Remove the source prefix already represented by an evaluation error's location fields.
fn strip_runtime_location(message: String, chunk_name: &str, line: Option<usize>) -> String {
    let Some(line) = line else {
        return message;
    };
    let prefix = format!("{}:{line}:", normalize_chunk_name(chunk_name));
    if let Some(rest) = message.strip_prefix(&prefix) {
        return rest.trim_start().to_owned();
    }
    message
}

fn runtime_structured_kind(error: &MarshaledScriptError) -> Option<StructuredErrorKind> {
    match error.value().str_field("kind").ok().flatten() {
        Some("io") => Some(StructuredErrorKind::Io),
        _ => None,
    }
}

fn timeout_message(timeout: Duration) -> String {
    format!("script timed out after {}ms", timeout.as_millis())
}

fn async_context_error(chunk_name: &str, source: &str) -> Error {
    Error::new(
        ErrorKind::AsyncContext,
        chunk_name,
        source,
        None,
        None,
        "blocking evaluation called from a Tokio context that cannot block \
         (a current-thread runtime or a LocalSet); use the async \
         `eval`/`eval_checked` methods instead",
    )
}

#[cfg(any())]
mod tests {
    use ruau_source::InMemorySource;
    use serde_json::json;

    use super::*;

    #[test]
    fn eval_installs_nested_json_args() {
        let host = Evaluator::new(Surface::new());
        let args = json!({
            "payload": {
                "name": "probe",
                "values": [7, "eight"],
                "meta": { "enabled": true }
            }
        });

        let outcome = host
            .eval_blocking(
                "local payload = args.payload\nlocal meta = payload.meta\nreturn { name = payload.name, first = payload.values[1], enabled = meta.enabled }",
                Options::trusted().args(args),
            )
            .expect("nested JSON args evaluate");

        assert_eq!(
            outcome.value,
            Some(json!({ "name": "probe", "first": 7, "enabled": true }))
        );
    }

    #[test]
    fn eval_args_integers_compare_and_add_as_numbers() {
        let host = Evaluator::new(Surface::new());
        let outcome = host
            .eval_blocking(
                "return args.n == 1 and args.n + 1 == 2 and type(args.n) == 'number'",
                Options::trusted().args(json!({"n": 1})),
            )
            .expect("integer args evaluate as numbers");
        assert_eq!(outcome.value, Some(json!(true)));

        let outcome = host
            .eval_blocking(
                "return type(args.n)",
                Options::trusted().args(json!({"n": 9_007_199_254_740_993i64})),
            )
            .expect("oversized integer args stay integers");
        assert_eq!(outcome.value, Some(json!("integer")));
    }

    #[test]
    fn eval_args_preserve_document_nulls_and_empty_arrays() {
        let host = Evaluator::new(Surface::new());
        let outcome = host
            .eval_blocking(
                "return type(args.explicit), args.missing == nil, args.explicit == nil, args.explicit == args.nested.value, args.empty, args.nested.empty",
                Options::trusted().args(json!({
                    "explicit": null,
                    "empty": [],
                    "nested": { "value": null, "empty": [] }
                })),
            )
            .expect("JSON document args evaluate");

        assert_eq!(
            outcome.value,
            Some(json!(["userdata", true, false, true, [], []]))
        );
    }

    #[test]
    fn eval_uses_source_load_name_for_runtime_locations() {
        let host = Evaluator::new(Surface::new());

        let error = host
            .eval_blocking(
                "local function fail()\n    error('boom')\nend\nfail()",
                Options::trusted().chunk_name("scripts/eval.luau"),
            )
            .expect_err("script raises a runtime error");

        assert_eq!(error.kind, ErrorKind::Runtime);
        assert_eq!(error.chunk_name, "scripts/eval.luau");
        assert!(
            error.line.is_some(),
            "ordinary chunk names should map back from @-normalized runtime frames"
        );
        assert_eq!(error.message, "boom");
    }

    #[test]
    fn eval_retains_structured_script_error_details() {
        let host = Evaluator::new(Surface::new());

        let error = host
            .eval_blocking(
                "error({ kind = 'validation', op = 'open', field = 'width', message = 'width must be positive' })",
                Options::trusted(),
            )
            .expect_err("script raises a structured error");

        assert_eq!(error.kind, ErrorKind::Runtime);
        assert_eq!(
            error.structured,
            Some(Box::new(json!({
                "kind": "validation",
                "op": "open",
                "field": "width",
                "message": "width must be positive",
            })))
        );
    }

    #[test]
    fn eval_reports_script_facing_marshal_paths() {
        let host = Evaluator::new(Surface::new());

        let string_key = host
            .eval_blocking(
                "local value = {}; value.self = value; return value",
                Options::trusted(),
            )
            .expect_err("cycle through a string key is rejected");
        assert_eq!(string_key.kind, ErrorKind::Marshal);
        assert!(
            string_key.message.contains("$[1].self"),
            "unexpected marshal path: {}",
            string_key.message
        );

        let integer_key = host
            .eval_blocking(
                "local value = {}; value[1] = value; return value",
                Options::trusted(),
            )
            .expect_err("cycle through an integer key is rejected");
        assert_eq!(integer_key.kind, ErrorKind::Marshal);
        assert!(
            integer_key.message.contains("$[1][1]"),
            "unexpected marshal path: {}",
            integer_key.message
        );
    }

    #[test]
    fn checked_eval_uses_module_id_for_relative_dependencies() {
        let modules =
            Arc::new(InMemorySource::new().with_module(ModuleId::new("app/dep"), "return 41"));
        let surface = Surface::builder()
            .module_source(modules)
            .build()
            .expect("surface validates");
        let host = Evaluator::new(surface);

        let outcome = host
            .eval_checked_blocking(
                "return require('./dep') + 1",
                Options::trusted()
                    .chunk_name("/tmp/main.luau")
                    .requester(ModuleId::new("app/main")),
            )
            .expect("relative dependency resolves from module id");

        assert_eq!(outcome.value, Some(Value::from(42)));

        let error = host
            .eval_checked_blocking(
                "require('./dep')\nerror('boom')",
                Options::trusted()
                    .chunk_name("/tmp/main.luau")
                    .requester(ModuleId::new("app/main")),
            )
            .expect_err("script raises after resolving its dependency");
        assert_eq!(error.chunk_name, "/tmp/main.luau");
        assert_eq!(error.line, Some(2));
    }

    #[test]
    fn default_options_apply_the_bounded_untrusted_policy() {
        let options = Options::default();

        assert_eq!(options.timeout, Some(DEFAULT_TIMEOUT));
        assert_eq!(options.limits.gas, Some(DEFAULT_GAS));
        assert_eq!(
            options.limits.max_memory_bytes,
            Some(DEFAULT_MAX_MEMORY_BYTES)
        );
        assert_eq!(options.print_quota.max_bytes, Some(DEFAULT_PRINT_MAX_BYTES));
        assert_eq!(options.print_quota.max_calls, Some(DEFAULT_PRINT_MAX_CALLS));
    }

    #[test]
    fn trusted_options_are_unmetered_and_executable() {
        let host = Evaluator::new(Surface::new());
        let options = Options::trusted();

        assert_eq!(options.timeout, None);
        assert_eq!(options.limits.gas, None);
        assert_eq!(options.limits.max_memory_bytes, None);
        let outcome = host
            .eval_blocking("return 7", options)
            .expect("trusted options do not meter gas");
        assert_eq!(outcome.value.and_then(|value| value.as_f64()), Some(7.0));
    }

    #[test]
    fn eval_blocking_needs_no_caller_tokio_setup_and_drives_wall_timeouts() {
        let host = Evaluator::new(Surface::new());

        // The wall deadline is a tokio timer inside the VM future, so this
        // also proves the lazily created blocking runtime has time enabled.
        let error = host
            .eval_blocking(
                "while true do end",
                Options::default()
                    .timeout(Duration::from_millis(20))
                    .limits(Limits::unlimited()),
            )
            .expect_err("the wall timeout terminates a runaway loop");
        assert_eq!(error.kind, ErrorKind::Timeout, "{}", error.message);

        // The cached runtime is reused for later evaluations.
        let outcome = host
            .eval_blocking("return 7", Options::default())
            .expect("the cached blocking runtime evaluates again");
        assert_eq!(outcome.value, Some(Value::from(7)));
    }

    #[test]
    fn eval_blocking_inside_a_current_thread_runtime_errors_instead_of_panicking() {
        let host = Evaluator::new(Surface::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime builds");

        let error = runtime
            .block_on(async { host.eval_blocking("return 1", Options::default()) })
            .expect_err("blocking eval refuses to run inside a current-thread runtime");
        assert_eq!(error.kind, ErrorKind::AsyncContext);

        let checked = runtime
            .block_on(async { host.eval_checked_blocking("return 1", Options::default()) })
            .expect_err("checked blocking eval refuses to run inside a current-thread runtime");
        assert_eq!(checked.kind, ErrorKind::AsyncContext);

        // The async entry points remain the supported path there.
        let outcome = runtime
            .block_on(host.eval("return 2", Options::default()))
            .expect("async eval runs inside the ambient runtime");
        assert_eq!(outcome.value, Some(Value::from(2)));
    }

    #[test]
    fn eval_blocking_inside_a_multi_thread_runtime_blocks_in_place() {
        let host = Arc::new(Evaluator::new(Surface::new()));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .expect("test runtime builds");

        // From an async task on a multi-thread runtime.
        let outcome = runtime
            .block_on(async { host.eval_blocking("return 3", Options::default()) })
            .expect("blocking eval blocks in place on a multi-thread runtime");
        assert_eq!(outcome.value, Some(Value::from(3)));

        // From a spawn_blocking closure — the async-to-sync bridge externals use.
        let bridged = Arc::clone(&host);
        let outcome = runtime
            .block_on(async {
                tokio::task::spawn_blocking(move || {
                    bridged.eval_blocking("return 4", Options::default())
                })
                .await
                .expect("spawn_blocking join succeeds")
            })
            .expect("blocking eval runs inside spawn_blocking");
        assert_eq!(outcome.value, Some(Value::from(4)));

        // A LocalSet cannot block; it degrades to the structured error.
        let error = runtime
            .block_on(async {
                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async { host.eval_blocking("return 5", Options::default()) })
                    .await
            })
            .expect_err("blocking eval refuses to run inside a LocalSet");
        assert_eq!(error.kind, ErrorKind::AsyncContext);
    }

    #[test]
    fn blocking_runtime_context_permits_block_in_place_bridging() {
        // Native module callbacks run inline while the VM future is polled on
        // the blocking runtime; external modules (itty's term module) bridge
        // sync-to-async there with `block_in_place`, which panics under a
        // current-thread flavor. Pin the multi-thread-flavored contract.
        let host = Evaluator::new(Surface::new());
        let value = host
            .blocking_runtime
            .block_on(async { tokio::task::block_in_place(|| 7) })
            .expect("blocking runtime builds");
        assert_eq!(value, 7);
    }

    #[test]
    fn evaluator_with_cached_runtime_drops_safely_inside_an_async_context() {
        let host = Evaluator::new(Surface::new());
        host.eval_blocking("return 1", Options::default())
            .expect("first eval initializes the cached blocking runtime");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime builds");
        runtime.block_on(async move { drop(host) });
    }
}
