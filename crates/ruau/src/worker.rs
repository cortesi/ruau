//! Dedicated Tokio worker for multi-threaded applications.
//!
//! Direct [`crate::Luau`] handles are local to one thread and intentionally `!Send + !Sync`.
//! `LuauWorker` owns one VM on a dedicated OS thread and exposes a cloneable
//! [`LuauWorkerHandle`] that can be used from ordinary Tokio tasks.

use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt, future,
    panic::Location,
    pin::Pin,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc as std_mpsc,
    },
    thread,
};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::{
    AsChunk, Compiler, FromLuauMulti, IntoLuauMulti, Luau, LuauOptions, Result, StdLib,
    error::Error as LuauError,
};

/// Result type returned by worker APIs.
pub type LuauWorkerResult<T> = std::result::Result<T, LuauWorkerError>;

/// Error type used across the worker boundary.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum LuauWorkerError {
    /// The VM returned a local `ruau::Error`, converted to a `Send` representation.
    #[error("Luau {kind} error: {message}")]
    Vm {
        /// Stable category for the VM error.
        kind: &'static str,
        /// Rendered error message.
        message: String,
    },
    /// A value failed conversion at the worker boundary.
    #[error("worker conversion error: {0}")]
    Conversion(String),
    /// The caller dropped the response future before completion, or the VM task was aborted.
    #[error("worker request was cancelled")]
    Cancelled,
    /// The worker is no longer accepting requests.
    #[error("worker has shut down")]
    Shutdown,
    /// A VM-lane task panicked.
    #[error("worker task panicked: {0}")]
    Panicked(String),
    /// A blocking Tokio task failed to join.
    #[error("blocking task join failed: {0}")]
    JoinFailed(String),
    /// The worker thread or runtime failed to start.
    #[error("worker runtime failed: {0}")]
    Runtime(String),
}

impl LuauWorkerError {
    /// Converts a non-`Send` Luau error into the worker error representation.
    #[must_use]
    pub fn from_luau(error: LuauError) -> Self {
        match error {
            LuauError::FromLuauConversionError { .. } => Self::Conversion(error.to_string()),
            LuauError::BadArgument { .. } => Self::Conversion(error.to_string()),
            other => Self::Vm {
                kind: error_kind(&other),
                message: other.to_string(),
            },
        }
    }
}

impl From<LuauError> for LuauWorkerError {
    fn from(error: LuauError) -> Self {
        Self::from_luau(error)
    }
}

fn error_kind(error: &LuauError) -> &'static str {
    match error {
        LuauError::SyntaxError { .. } => "syntax",
        LuauError::RuntimeError(_) => "runtime",
        LuauError::MemoryError(_) => "memory",
        LuauError::SafetyError(_) => "safety",
        LuauError::SerializeError(_) => "serialize",
        LuauError::DeserializeError(_) => "deserialize",
        LuauError::AsyncCallbackCancelled => "cancelled",
        LuauError::ExternalError(_) => "external",
        LuauError::WithContext { .. } => "context",
        _ => "runtime",
    }
}

type WorkerValue = Box<dyn Any + Send>;
type WorkerFuture<'lua> = Pin<Box<dyn Future<Output = LuauWorkerResult<WorkerValue>> + 'lua>>;
type WorkerJob = Box<dyn for<'lua> FnOnce(&'lua Luau) -> WorkerFuture<'lua> + Send>;
type SetupFn = Box<dyn FnOnce(&Luau) -> Result<()> + Send + 'static>;

struct WorkerRequest {
    id: u64,
    job: WorkerJob,
    response: oneshot::Sender<LuauWorkerResult<WorkerValue>>,
}

enum WorkerControl {
    Cancel(u64),
    Shutdown(oneshot::Sender<()>),
}

/// Builder for a dedicated Luau worker thread.
pub struct LuauWorkerBuilder {
    std_libs: StdLib,
    options: LuauOptions,
    compiler: Option<Compiler>,
    channel_capacity: usize,
    thread_name: String,
    setup: Vec<SetupFn>,
}

impl fmt::Debug for LuauWorkerBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LuauWorkerBuilder")
            .field("std_libs", &self.std_libs)
            .field("options", &self.options)
            .field("compiler", &self.compiler)
            .field("channel_capacity", &self.channel_capacity)
            .field("thread_name", &self.thread_name)
            .field("setup_len", &self.setup.len())
            .finish()
    }
}

impl Default for LuauWorkerBuilder {
    fn default() -> Self {
        Self {
            std_libs: StdLib::ALL_SAFE,
            options: LuauOptions::default(),
            compiler: None,
            channel_capacity: 128,
            thread_name: "ruau-worker".to_owned(),
            setup: Vec::new(),
        }
    }
}

impl LuauWorkerBuilder {
    /// Sets the standard libraries loaded into the worker VM.
    #[must_use]
    pub fn std_libs(mut self, libs: StdLib) -> Self {
        self.std_libs = libs;
        self
    }

    /// Sets Luau VM options.
    #[must_use]
    pub const fn options(mut self, options: LuauOptions) -> Self {
        self.options = options;
        self
    }

    /// Sets the default compiler used by the worker VM.
    #[must_use]
    pub fn compiler(mut self, compiler: Compiler) -> Self {
        self.compiler = Some(compiler);
        self
    }

    /// Sets the bounded request-channel capacity.
    #[must_use]
    pub const fn channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    /// Sets the dedicated worker thread name.
    #[must_use]
    pub fn thread_name(mut self, name: impl Into<String>) -> Self {
        self.thread_name = name.into();
        self
    }

    /// Adds a setup closure that runs on the VM lane before the worker starts accepting requests.
    #[must_use]
    pub fn with_setup<F>(mut self, setup: F) -> Self
    where
        F: FnOnce(&Luau) -> Result<()> + Send + 'static,
    {
        self.setup.push(Box::new(setup));
        self
    }

    /// Spawns the worker thread and returns its owner.
    pub fn build(self) -> LuauWorkerResult<LuauWorker> {
        let capacity = self.channel_capacity.max(1);
        let (request_tx, request_rx) = mpsc::channel(capacity);
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let next_id = Arc::new(AtomicU64::new(1));
        let (init_tx, init_rx) = std_mpsc::channel();

        let handle = LuauWorkerHandle {
            request_tx,
            control_tx,
            closed: Arc::clone(&closed),
            next_id,
        };
        let thread_handle = handle.clone();

        let thread = thread::Builder::new()
            .name(self.thread_name)
            .spawn(move || {
                let init = WorkerInit {
                    std_libs: self.std_libs,
                    options: self.options,
                    compiler: self.compiler,
                    setup: self.setup,
                    request_rx,
                    control_rx,
                    init_tx,
                };
                run_worker_thread(init);
            })
            .map_err(|error| LuauWorkerError::Runtime(error.to_string()))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(LuauWorker {
                handle: thread_handle,
                join: Some(thread),
                closed,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                let _ = thread.join();
                Err(LuauWorkerError::Runtime(error.to_string()))
            }
        }
    }
}

struct WorkerInit {
    std_libs: StdLib,
    options: LuauOptions,
    compiler: Option<Compiler>,
    setup: Vec<SetupFn>,
    request_rx: mpsc::Receiver<WorkerRequest>,
    control_rx: mpsc::UnboundedReceiver<WorkerControl>,
    init_tx: std_mpsc::Sender<LuauWorkerResult<()>>,
}

fn run_worker_thread(init: WorkerInit) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = init
                .init_tx
                .send(Err(LuauWorkerError::Runtime(error.to_string())));
            return;
        }
    };

    let local = tokio::task::LocalSet::new();
    runtime.block_on(local.run_until(async move {
        let lua = match Luau::new_with(init.std_libs, init.options) {
            Ok(lua) => Rc::new(lua),
            Err(error) => {
                let _ = init.init_tx.send(Err(error.into()));
                return;
            }
        };
        if let Some(compiler) = init.compiler {
            lua.set_compiler(compiler);
        }
        for setup in init.setup {
            if let Err(error) = setup(&lua) {
                let _ = init.init_tx.send(Err(error.into()));
                return;
            }
        }
        let _ = init.init_tx.send(Ok(()));
        run_worker_loop(lua, init.request_rx, init.control_rx).await;
    }));
}

async fn run_worker_loop(
    lua: Rc<Luau>,
    mut request_rx: mpsc::Receiver<WorkerRequest>,
    mut control_rx: mpsc::UnboundedReceiver<WorkerControl>,
) {
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<u64>();
    let mut in_flight: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    let mut cancelled_before_spawn = HashSet::new();
    let mut shutdown: Option<oneshot::Sender<()>> = None;
    let mut accepting = true;

    loop {
        if !accepting && in_flight.is_empty() {
            if let Some(sender) = shutdown.take() {
                let _ = sender.send(());
            }
            break;
        }

        tokio::select! {
            Some(id) = done_rx.recv() => {
                in_flight.remove(&id);
            }
            Some(control) = control_rx.recv() => {
                match control {
                    WorkerControl::Cancel(id) => {
                        if let Some(handle) = in_flight.get(&id) {
                            handle.abort();
                        } else {
                            cancelled_before_spawn.insert(id);
                        }
                    }
                    WorkerControl::Shutdown(sender) => {
                        accepting = false;
                        shutdown = Some(sender);
                        while let Ok(request) = request_rx.try_recv() {
                            spawn_or_cancel_request(
                                Rc::clone(&lua),
                                request,
                                &mut in_flight,
                                &mut cancelled_before_spawn,
                                done_tx.clone(),
                            );
                        }
                    }
                }
            }
            request = request_rx.recv(), if accepting => {
                match request {
                    Some(request) => spawn_or_cancel_request(
                        Rc::clone(&lua),
                        request,
                        &mut in_flight,
                        &mut cancelled_before_spawn,
                        done_tx.clone(),
                    ),
                    None => accepting = false,
                }
            }
            else => {
                accepting = false;
            }
        }
    }
}

fn spawn_or_cancel_request(
    lua: Rc<Luau>,
    request: WorkerRequest,
    in_flight: &mut HashMap<u64, tokio::task::AbortHandle>,
    cancelled_before_spawn: &mut HashSet<u64>,
    done_tx: mpsc::UnboundedSender<u64>,
) {
    let id = request.id;
    if cancelled_before_spawn.remove(&id) {
        let _ = request.response.send(Err(LuauWorkerError::Cancelled));
        return;
    }

    let WorkerRequest { job, response, .. } = request;
    let task = tokio::task::spawn_local(async move { job(&lua).await });
    let abort_handle = task.abort_handle();
    in_flight.insert(id, abort_handle);
    tokio::task::spawn_local(async move {
        let result = match task.await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Err(LuauWorkerError::Cancelled),
            Err(error) => Err(LuauWorkerError::Panicked(error.to_string())),
        };
        let _ = response.send(result);
        let _ = done_tx.send(id);
    });
}

/// Owner for one dedicated Luau worker.
pub struct LuauWorker {
    handle: LuauWorkerHandle,
    join: Option<thread::JoinHandle<()>>,
    closed: Arc<AtomicBool>,
}

impl LuauWorker {
    /// Creates a worker builder.
    #[must_use]
    pub fn builder() -> LuauWorkerBuilder {
        LuauWorkerBuilder::default()
    }

    /// Clones the worker handle.
    #[must_use]
    pub fn handle(&self) -> LuauWorkerHandle {
        self.handle.clone()
    }

    /// Stops accepting new work, waits for accepted requests to drain, and joins the worker thread.
    pub async fn shutdown(mut self) -> LuauWorkerResult<()> {
        self.closed.store(true, Ordering::Release);
        let (tx, rx) = oneshot::channel();
        self.handle
            .control_tx
            .send(WorkerControl::Shutdown(tx))
            .map_err(|_| LuauWorkerError::Shutdown)?;
        rx.await.map_err(|_| LuauWorkerError::Shutdown)?;
        if let Some(join) = self.join.take() {
            tokio::task::spawn_blocking(move || join.join())
                .await
                .map_err(|error| LuauWorkerError::JoinFailed(error.to_string()))?
                .map_err(panic_payload_to_error)?;
        }
        Ok(())
    }
}

impl Drop for LuauWorker {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let (tx, rx) = oneshot::channel();
            let _ = self.handle.control_tx.send(WorkerControl::Shutdown(tx));
            drop(rx);
            let _ = join.join();
        }
    }
}

/// Cloneable `Send + Sync` handle to a dedicated Luau worker.
#[derive(Clone)]
pub struct LuauWorkerHandle {
    request_tx: mpsc::Sender<WorkerRequest>,
    control_tx: mpsc::UnboundedSender<WorkerControl>,
    closed: Arc<AtomicBool>,
    next_id: Arc<AtomicU64>,
}

impl fmt::Debug for LuauWorkerHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("LuauWorkerHandle").finish_non_exhaustive()
    }
}

impl LuauWorkerHandle {
    /// Runs a local async VM-lane closure and returns an owned `Send` result.
    pub async fn with_async<R, F>(&self, f: F) -> LuauWorkerResult<R>
    where
        R: Send + 'static,
        F: for<'lua> FnOnce(&'lua Luau) -> Pin<Box<dyn Future<Output = Result<R>> + 'lua>> + Send + 'static,
    {
        self.submit(Box::new(move |lua| {
            Box::pin(async move {
                f(lua)
                    .await
                    .map(|value| Box::new(value) as WorkerValue)
                    .map_err(LuauWorkerError::from)
            })
        }))
        .await
    }

    /// Runs a short synchronous VM-lane closure.
    pub async fn with<R, F>(&self, f: F) -> LuauWorkerResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&Luau) -> Result<R> + Send + 'static,
    {
        self.with_async(move |lua| Box::pin(future::ready(f(lua)))).await
    }

    /// Executes an in-memory Luau chunk.
    pub async fn exec<C>(&self, source: C) -> LuauWorkerResult<()>
    where
        C: AsChunk + Send + 'static,
    {
        let caller = Location::caller();
        self.with_async(move |lua| {
            Box::pin(async move { lua.load_with_location(source, caller).exec().await })
        })
        .await
    }

    /// Evaluates an in-memory Luau chunk.
    pub async fn eval<R, C>(&self, source: C) -> LuauWorkerResult<R>
    where
        R: FromLuauMulti + Send + 'static,
        C: AsChunk + Send + 'static,
    {
        let caller = Location::caller();
        self.with_async(move |lua| {
            Box::pin(async move { lua.load_with_location(source, caller).eval().await })
        })
        .await
    }

    /// Calls a global Luau function by name.
    pub async fn call<R, A>(&self, global_name: impl Into<String>, args: A) -> LuauWorkerResult<R>
    where
        R: FromLuauMulti + Send + 'static,
        A: IntoLuauMulti + Send + 'static,
    {
        let global_name = global_name.into();
        self.with_async(move |lua| {
            Box::pin(async move {
                let function: crate::Function = lua.globals().get(global_name)?;
                function.call(args).await
            })
        })
        .await
    }

    async fn submit<R>(&self, job: WorkerJob) -> LuauWorkerResult<R>
    where
        R: Send + 'static,
    {
        if self.closed.load(Ordering::Acquire) {
            return Err(LuauWorkerError::Shutdown);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.request_tx
            .send(WorkerRequest {
                id,
                job,
                response: response_tx,
            })
            .await
            .map_err(|_| LuauWorkerError::Shutdown)?;

        let mut guard = CancelOnDrop {
            id,
            sender: self.control_tx.clone(),
            armed: true,
        };
        let value = response_rx.await.map_err(|_| LuauWorkerError::Shutdown)??;
        guard.armed = false;
        value
            .downcast::<R>()
            .map(|boxed| *boxed)
            .map_err(|_| LuauWorkerError::Conversion("worker response type mismatch".to_owned()))
    }
}

struct CancelOnDrop {
    id: u64,
    sender: mpsc::UnboundedSender<WorkerControl>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.sender.send(WorkerControl::Cancel(self.id));
        }
    }
}

fn panic_payload_to_error(payload: Box<dyn Any + Send + 'static>) -> LuauWorkerError {
    if let Some(message) = payload.downcast_ref::<&str>() {
        LuauWorkerError::Panicked((*message).to_owned())
    } else if let Some(message) = payload.downcast_ref::<String>() {
        LuauWorkerError::Panicked(message.clone())
    } else {
        LuauWorkerError::Panicked("unknown panic payload".to_owned())
    }
}
