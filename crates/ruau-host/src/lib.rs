//! Retained source-eval host over a validated [`Surface`].

use std::{
    any::Any,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    error::Error as StdError,
    fmt,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ruau_bytecode::{BytecodeChunk, CompileError, CompileOptions};
use ruau_source::{ModuleName, Source};
use ruau_surface::{Surface, VmConfig};
use ruau_vm::{
    Ambient, CallOptions, Cancel, Deadline, ExecError, Limits, MarshaledScriptError,
    MarshaledValue, SinkQuota, VmBuildError,
};
use ruau_vm_api::{
    ModuleBinding, ModuleBuilder, ModuleTable, ModuleValue, NativeModule, RuntimeErrorKind,
};
use serde_json::{Map, Number, Value};

const DEFAULT_CHUNK_NAME: &str = "eval.luau";
const DEFAULT_PRINT_MAX_BYTES: usize = 64 * 1024;
const DEFAULT_PRINT_MAX_CALLS: usize = 1024;
/// Default wall-clock timeout for retained evaluation of untrusted source.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);

#[cfg(any())]
static ACTIVE_TIMEOUT_TIMER_THREADS: AtomicU64 = AtomicU64::new(0);

static HOST_TIMEOUT_TIMER: OnceLock<TimeoutTimer> = OnceLock::new();

/// Retained source evaluator for ordinary embedding hosts.
pub struct Evaluator {
    surface: Surface,
    compile_policy: CompileOptions,
    handle: tokio::runtime::Handle,
    next_seed: AtomicU64,
}

impl Evaluator {
    /// Builds a host over a validated surface and a Tokio runtime handle.
    #[must_use]
    pub fn new(surface: Surface, handle: tokio::runtime::Handle) -> Self {
        Self {
            surface,
            compile_policy: CompileOptions::default(),
            handle,
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

    /// Evaluates source on the retained host, blocking on the configured Tokio
    /// runtime handle.
    ///
    /// # Errors
    /// Returns [`Error`] for argument conversion, VM construction,
    /// compilation, loading, runtime, cancellation, timeout, and JSON result
    /// conversion failures.
    pub fn eval_blocking(&self, source: &str, options: Options) -> Result<Outcome, Error> {
        self.handle.block_on(self.eval(source, options))
    }

    /// Checks and evaluates source on the retained host, blocking on the
    /// configured Tokio runtime handle.
    ///
    /// # Errors
    /// Returns [`Error`] for static checking, argument conversion, VM
    /// construction, compilation, loading, runtime, cancellation, timeout, and
    /// JSON result conversion failures.
    pub fn eval_checked_blocking(&self, source: &str, options: Options) -> Result<Outcome, Error> {
        self.handle.block_on(self.eval_checked(source, options))
    }

    /// Evaluates source on the async VM driver.
    ///
    /// # Errors
    /// Returns [`Error`] for argument conversion, VM construction,
    /// compilation, loading, runtime, cancellation, timeout, and JSON result
    /// conversion failures.
    pub async fn eval(&self, source: &str, options: Options) -> Result<Outcome, Error> {
        let started = Instant::now();
        let source_text = source.to_owned();
        let script = Source::text(options.chunk_name.clone(), source_text.clone());
        let chunk_name = script.display_name().to_owned();
        let compiled = self.compile_eval_script(script, source_text, chunk_name)?;
        self.exec_compiled(compiled, options, started).await
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
    pub async fn eval_checked(&self, source: &str, options: Options) -> Result<Outcome, Error> {
        let started = Instant::now();
        let source_text = source.to_owned();
        let script = Source::text(options.chunk_name.clone(), source_text.clone());
        let chunk_name = script.display_name().to_owned();

        self.check_eval_source(&script, &chunk_name, &source_text)
            .await?;

        let compiled = self.compile_eval_script(script, source_text, chunk_name)?;
        self.exec_compiled(compiled, options, started).await
    }

    async fn check_eval_source(
        &self,
        script: &Source,
        chunk_name: &str,
        source_text: &str,
    ) -> Result<(), Error> {
        if self.surface.has_module_source() {
            let graph = self.surface.check_source_graph_async(script).await;
            if graph.has_errors() {
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
                return Err(Error::new(
                    ErrorKind::Check,
                    chunk_name,
                    source_text,
                    line,
                    column,
                    diagnostics.render(),
                ));
            }
            return Ok(());
        }

        let checked = self.surface.check(script);
        if checked.has_errors() {
            let root_location = checked.diagnostics().first().map(|diagnostic| {
                let begin = diagnostic.primary_location.begin;
                (begin.line, begin.column)
            });
            let (line, column) = diagnostic_line_column(root_location);
            return Err(Error::new(
                ErrorKind::Check,
                chunk_name,
                source_text,
                line,
                column,
                checked.diagnostics().render(script.display_name()),
            ));
        }
        Ok(())
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
            .compile_source_with_options(&script, &self.compile_policy)
            .map_err(|error| Error::from_compile(&chunk_name, &source_text, &error))?;
        Ok(CompiledEval {
            script,
            source_text,
            chunk_name,
            chunk,
            compile: compile_start.elapsed(),
        })
    }

    async fn exec_compiled(
        &self,
        compiled: CompiledEval,
        options: Options,
        started: Instant,
    ) -> Result<Outcome, Error> {
        let CompiledEval {
            script,
            source_text,
            chunk_name,
            chunk,
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
                Limits::unlimited(),
            ))
            .module(Arc::new(GlobalValueModule::new("args", args)))
            .build()
            .map_err(|error| Error::from_build(&chunk_name, &source_text, error))?;
        let load_name = script.load_name();
        let module = vm
            .load_named_module(&chunk, script.id().clone(), &load_name)
            .map_err(|error| {
                Error::new(
                    ErrorKind::Load,
                    &chunk_name,
                    &source_text,
                    None,
                    None,
                    format!("script load failed: {error}"),
                )
            })?;

        let prints = Arc::new(Mutex::new(Vec::<String>::new()));
        let (limits, _timeout_guard) = limits_for_eval(
            options.timeout,
            options.cancel.clone(),
            host_timeout_timer(),
        );
        let mut call_options = CallOptions::new()
            .limits(limits)
            .print_sink_with_quota(print_sink(Arc::clone(&prints)), options.print_quota);
        for value in options.app_data {
            call_options = call_options.app_data_erased(value);
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

        Ok(Outcome {
            value,
            prints,
            timing: Timing {
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
    script: Source,
    source_text: String,
    chunk_name: String,
    chunk: BytecodeChunk,
    compile: Duration,
}

/// Per-evaluation controls.
///
/// The default is the untrusted-source posture: output is quota-limited and
/// execution is wall-clock bounded by [`DEFAULT_TIMEOUT`]. Use
/// [`Options::trusted`] or [`Options::without_timeout`] only for source whose
/// CPU use is controlled by the embedding host.
pub struct Options {
    /// Chunk name used for loading, traceback frames, and errors.
    pub chunk_name: String,
    /// Wall-clock timeout. When set, the host installs both a wall deadline and
    /// a cancellation watchdog. Defaults to [`DEFAULT_TIMEOUT`].
    pub timeout: Option<Duration>,
    /// External cancellation signal for this evaluation.
    pub cancel: Option<Cancel>,
    /// JSON-shaped global installed as `args` before sandboxing.
    pub args: Value,
    app_data: Vec<Box<dyn Any + Send + Sync>>,
    /// Per-evaluation print quota.
    pub print_quota: SinkQuota,
}

impl Options {
    /// Builds explicitly trusted-source controls with no wall-clock timeout.
    #[must_use]
    pub fn trusted() -> Self {
        Self::default().without_timeout()
    }

    /// Sets the chunk name used in runtime tracebacks and diagnostics.
    #[must_use]
    pub fn chunk_name(mut self, name: impl Into<String>) -> Self {
        self.chunk_name = name.into();
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
    /// Prefer this only when the source is trusted or independently bounded by
    /// the host.
    #[must_use]
    pub fn without_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// Sets an external cancellation signal for this evaluation.
    #[must_use]
    pub fn cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Sets the JSON value installed as the `args` global.
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
}

impl Default for Options {
    fn default() -> Self {
        Self {
            chunk_name: DEFAULT_CHUNK_NAME.to_owned(),
            timeout: Some(DEFAULT_TIMEOUT),
            cancel: None,
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
            .field("timeout", &self.timeout)
            .field("cancel", &self.cancel)
            .field("args", &self.args)
            .field("app_data_len", &self.app_data.len())
            .field("print_quota", &self.print_quota)
            .finish()
    }
}

/// Successful evaluation output.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// JSON return value: `None` for no returns, one JSON value for one return,
    /// or a JSON array for multiple returns.
    pub value: Option<Value>,
    /// Captured `print` output lines.
    pub prints: Vec<String>,
    /// Compile, execute, and total wall timings.
    pub timing: Timing,
}

/// Evaluation timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timing {
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
    /// JSON args could not be represented as module constants.
    Args,
    /// Static checking produced error-severity diagnostics.
    Check,
    /// VM construction failed.
    Build,
    /// Source compilation failed.
    Compile,
    /// Bytecode loading failed.
    Load,
    /// Script raised a catchable runtime error.
    Runtime,
    /// Evaluation was cancelled.
    Cancelled,
    /// Evaluation exceeded its timeout/deadline.
    Timeout,
    /// The VM was poisoned by a panic.
    PanicPoison,
    /// Return-value marshaling or JSON conversion failed.
    Marshal,
}

/// Structured evaluation error with source context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    /// Error category.
    pub kind: ErrorKind,
    /// Chunk name associated with the source.
    pub chunk_name: String,
    /// 1-based source line when known.
    pub line: Option<usize>,
    /// 1-based source column when known.
    pub column: Option<usize>,
    /// Human-readable message.
    pub message: String,
    /// Full source text.
    pub source: String,
}

impl Error {
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
                Self::new(
                    ErrorKind::Runtime,
                    chunk_name,
                    source,
                    line,
                    column,
                    runtime_message(error),
                )
            }
            ExecError::Cancelled => Self::new(
                ErrorKind::Cancelled,
                chunk_name,
                source,
                None,
                None,
                timeout.map_or_else(|| "script cancelled".to_owned(), timeout_message),
            ),
            ExecError::Deadline => Self::new(
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
        "ruau_host_globals"
    }

    fn declaration(&self) -> ruau_decl::DeclSource<'_> {
        ruau_decl::DeclSource::Text("")
    }

    fn build(&self, builder: &mut dyn ModuleBuilder) {
        builder.constant(&self.name, ModuleBinding::Global, self.value.clone());
    }
}

fn json_to_module_value(value: &Value) -> Result<ModuleValue, String> {
    match value {
        Value::Null => Ok(ModuleValue::Nil),
        Value::Bool(value) => Ok(ModuleValue::Boolean(*value)),
        Value::Number(number) => json_number_to_module_value(number),
        Value::String(value) => Ok(ModuleValue::Bytes(value.as_bytes().to_vec())),
        Value::Array(_) => {
            Err("JSON arrays cannot be installed as constant module globals".to_owned())
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
        return Ok(ModuleValue::Integer(value));
    }
    if let Some(value) = number.as_u64() {
        return i64::try_from(value)
            .map(ModuleValue::Integer)
            .map_err(|_| format!("JSON integer {value} exceeds Luau's i64 range"));
    }
    let value = number
        .as_f64()
        .ok_or_else(|| format!("JSON number {number} is not representable as f64"))?;
    Ok(ModuleValue::Number(value))
}

struct TimeoutTimer {
    sender: Option<mpsc::Sender<TimerCommand>>,
    next_id: AtomicU64,
}

impl TimeoutTimer {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let sender = thread::Builder::new()
            .name("ruau-host-timeout-timer".to_owned())
            .spawn(move || run_timeout_timer(&receiver))
            .map(|_| sender)
            .ok();
        Self {
            sender,
            next_id: AtomicU64::new(1),
        }
    }

    fn arm(&self, cancel: &Cancel, timeout: Duration) -> Option<TimeoutGuard> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        if deadline <= Instant::now() {
            cancel.cancel();
            return None;
        }

        if let Some(sender) = &self.sender {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            if sender
                .send(TimerCommand::Arm {
                    id,
                    deadline,
                    cancel: cancel.clone(),
                })
                .is_ok()
            {
                return Some(TimeoutGuard {
                    id,
                    sender: Some(sender.clone()),
                });
            }
        }

        arm_detached_cancel_after(cancel, timeout);
        None
    }
}

impl Drop for TimeoutTimer {
    fn drop(&mut self) {
        if let Some(sender) = &self.sender {
            drop(sender.send(TimerCommand::Stop));
        }
    }
}

enum TimerCommand {
    Arm {
        id: u64,
        deadline: Instant,
        cancel: Cancel,
    },
    Disarm(u64),
    Stop,
}

struct TimeoutGuard {
    id: u64,
    sender: Option<mpsc::Sender<TimerCommand>>,
}

impl Drop for TimeoutGuard {
    fn drop(&mut self) {
        if let Some(sender) = &self.sender {
            drop(sender.send(TimerCommand::Disarm(self.id)));
        }
    }
}

fn run_timeout_timer(receiver: &mpsc::Receiver<TimerCommand>) {
    #[cfg(any())]
    let _thread_count = TimeoutTimerThreadCount::new();

    let mut deadlines = BinaryHeap::<Reverse<(Instant, u64)>>::new();
    let mut cancels = HashMap::<u64, Cancel>::new();
    loop {
        cancel_expired_timers(&mut deadlines, &mut cancels);
        let command = match deadlines.peek().copied() {
            Some(Reverse((deadline, _))) => {
                let timeout = deadline.saturating_duration_since(Instant::now());
                receiver.recv_timeout(timeout)
            }
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match command {
            Ok(TimerCommand::Arm {
                id,
                deadline,
                cancel,
            }) => {
                if deadline <= Instant::now() {
                    cancel.cancel();
                } else {
                    deadlines.push(Reverse((deadline, id)));
                    cancels.insert(id, cancel);
                }
            }
            Ok(TimerCommand::Disarm(id)) => {
                cancels.remove(&id);
            }
            Ok(TimerCommand::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn cancel_expired_timers(
    deadlines: &mut BinaryHeap<Reverse<(Instant, u64)>>,
    cancels: &mut HashMap<u64, Cancel>,
) {
    let now = Instant::now();
    while let Some(Reverse((deadline, id))) = deadlines.peek().copied() {
        if deadline > now {
            break;
        }
        deadlines.pop();
        if let Some(cancel) = cancels.remove(&id) {
            cancel.cancel();
        }
    }
}

fn host_timeout_timer() -> &'static TimeoutTimer {
    HOST_TIMEOUT_TIMER.get_or_init(TimeoutTimer::new)
}

fn limits_for_eval(
    timeout: Option<Duration>,
    cancel: Option<Cancel>,
    timer: &TimeoutTimer,
) -> (Limits, Option<TimeoutGuard>) {
    let mut limits = Limits::unlimited();
    if let Some(timeout) = timeout {
        limits.deadline = Some(Deadline::Wall(
            Instant::now()
                .checked_add(timeout)
                .unwrap_or_else(Instant::now),
        ));
        let scoped_cancel = match cancel {
            Some(cancel) => cancel.child(),
            None => Cancel::manual(),
        };
        let guard = timer.arm(&scoped_cancel, timeout);
        limits.cancel = Some(scoped_cancel);
        return (limits, guard);
    } else {
        limits.cancel = cancel;
    }
    (limits, None)
}

fn arm_detached_cancel_after(cancel: &Cancel, timeout: Duration) {
    let cancel = cancel.clone();
    thread::Builder::new()
        .name("ruau-host-cancel-watchdog".to_owned())
        .spawn(move || {
            thread::sleep(timeout);
            cancel.cancel();
        })
        .ok();
}

#[cfg(any())]
struct TimeoutTimerThreadCount;

#[cfg(any())]
impl TimeoutTimerThreadCount {
    fn new() -> Self {
        ACTIVE_TIMEOUT_TIMER_THREADS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

#[cfg(any())]
impl Drop for TimeoutTimerThreadCount {
    fn drop(&mut self) {
        ACTIVE_TIMEOUT_TIMER_THREADS.fetch_sub(1, Ordering::SeqCst);
    }
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

fn eval_json_value(values: &[MarshaledValue]) -> Result<Option<Value>, String> {
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
    name.strip_prefix('@')
        .or_else(|| name.strip_prefix('='))
        .unwrap_or(name)
}

fn runtime_message(error: &MarshaledScriptError) -> String {
    if matches!(
        error.kind(),
        RuntimeErrorKind::Cancelled | RuntimeErrorKind::Deadline
    ) {
        return "script timed out".to_owned();
    }
    match error.value() {
        MarshaledValue::String(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        MarshaledValue::Nil => "nil".to_owned(),
        MarshaledValue::Boolean(value) => value.to_string(),
        MarshaledValue::Number(value) => value.to_string(),
        MarshaledValue::Integer(value) => value.to_string(),
        value => format!("script raised {}", value.type_name()),
    }
}

fn timeout_message(timeout: Duration) -> String {
    format!("script timed out after {}ms", timeout.as_millis())
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn eval_uses_source_load_name_for_runtime_locations() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime builds");
        let host = Evaluator::new(Surface::new(), runtime.handle().clone());

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
    }

    #[test]
    fn host_timeout_timer_reuses_one_thread_for_many_arms() {
        let timer = host_timeout_timer();
        wait_for_timer_threads_at_least(1);
        let active_after_start = ACTIVE_TIMEOUT_TIMER_THREADS.load(Ordering::SeqCst);
        assert_eq!(active_after_start, 1);

        let cancels = (0..128).map(|_| Cancel::manual()).collect::<Vec<_>>();
        let _guards = cancels
            .iter()
            .map(|cancel| timer.arm(cancel, Duration::from_millis(5)))
            .collect::<Vec<_>>();

        wait_until_all_cancelled(&cancels);
        assert_eq!(
            ACTIVE_TIMEOUT_TIMER_THREADS.load(Ordering::SeqCst),
            active_after_start
        );
    }

    fn wait_for_timer_threads_at_least(target: u64) {
        for _ in 0..100 {
            if ACTIVE_TIMEOUT_TIMER_THREADS.load(Ordering::SeqCst) >= target {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "timeout timer thread count did not reach {target}; active={}",
            ACTIVE_TIMEOUT_TIMER_THREADS.load(Ordering::SeqCst)
        );
    }

    fn wait_until_all_cancelled(cancels: &[Cancel]) {
        for _ in 0..100 {
            if cancels.iter().all(Cancel::is_cancelled) {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("shared timeout timer did not cancel every arm");
    }
}
