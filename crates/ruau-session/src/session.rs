use std::{
    error::Error as StdError,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

use ruau_bytecode::BytecodeChunk;
use ruau_surface::{PreparedGraph, Surface, VmConfig};
use ruau_vm::{CallOptions, ExecError, LoadError, ValueSnapshot, VmBuildError};

use crate::{
    BlockingRuntime, BlockingRuntimeError, LifecycleError, LoadTarget, RootHandle, Runtime,
};

const SESSION_THREAD_NAME: &str = "ruau-session-surface-session";

/// Retained VM session built from a validated [`Surface`].
pub struct SharedRuntime {
    surface: Arc<Surface>,
    runtime: Mutex<Runtime>,
    blocking_runtime: BlockingRuntime,
}

impl fmt::Debug for SharedRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedRuntime")
            .field("surface", &self.surface)
            .field("runtime_poisoned", &self.runtime.is_poisoned())
            .finish_non_exhaustive()
    }
}

/// Successful retained session execution.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedRuntimeOutcome {
    /// Values returned by the executed chunk.
    pub values: Vec<ValueSnapshot>,
    /// Host-initiated VM invocations performed by this run.
    pub execution_count: u64,
}

/// Error returned by retained session execution.
#[derive(Debug)]
pub enum SharedRuntimeError {
    /// The retained VM mutex was poisoned.
    Poisoned,
    /// The blocking runtime could not drive the VM future.
    Blocking(BlockingRuntimeError),
    /// The compiled chunk could not be loaded into the retained VM.
    Load(LoadError),
    /// The VM reported runtime control flow or marshaling failure.
    Exec {
        /// The underlying VM execution error.
        error: ExecError,
        /// Host-initiated VM invocations completed before the error surfaced.
        execution_count: u64,
    },
    /// The retained core rejected an internal handle or prepared context.
    Retained(LifecycleError),
}

impl SharedRuntimeError {
    /// Host-initiated VM invocations completed before this error surfaced.
    #[must_use]
    pub const fn execution_count(&self) -> u64 {
        match self {
            Self::Exec {
                execution_count, ..
            } => *execution_count,
            Self::Poisoned | Self::Blocking(_) | Self::Load(_) | Self::Retained(_) => 0,
        }
    }
}

impl fmt::Display for SharedRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned => formatter.write_str("retained surface session VM mutex was poisoned"),
            Self::Blocking(error) => write!(formatter, "{error}"),
            Self::Load(error) => write!(formatter, "retained surface session load failed: {error}"),
            Self::Exec { error, .. } => {
                write!(
                    formatter,
                    "retained surface session execution failed: {error}"
                )
            }
            Self::Retained(error) => write!(formatter, "retained surface session failed: {error}"),
        }
    }
}

impl StdError for SharedRuntimeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Poisoned => None,
            Self::Blocking(error) => Some(error),
            Self::Load(error) => Some(error),
            Self::Exec { error, .. } => Some(error),
            Self::Retained(error) => Some(error),
        }
    }
}

impl SharedRuntime {
    /// Builds a retained session from `surface` and VM configuration.
    ///
    /// # Errors
    /// Returns [`VmBuildError`] when the surface VM cannot be built.
    pub fn new(surface: Surface, config: &VmConfig) -> Result<Self, VmBuildError> {
        Self::with_blocking_runtime(surface, config, BlockingRuntime::new(SESSION_THREAD_NAME))
    }

    /// Builds a retained session with a caller-supplied blocking runtime.
    ///
    /// # Errors
    /// Returns [`VmBuildError`] when the surface VM cannot be built.
    pub fn with_blocking_runtime(
        surface: Surface,
        config: &VmConfig,
        blocking_runtime: BlockingRuntime,
    ) -> Result<Self, VmBuildError> {
        let surface = Arc::new(surface);
        let runtime = Runtime::with_shared_surface(Arc::clone(&surface), config)?;
        Ok(Self {
            surface,
            runtime: Mutex::new(runtime),
            blocking_runtime,
        })
    }

    /// Returns this session's surface.
    #[must_use]
    pub fn surface(&self) -> &Surface {
        self.surface.as_ref()
    }

    /// Runs a compiled chunk on the retained VM and returns owned values.
    ///
    /// This method reloads the chunk for each call, unloads it after execution, and clears the VM
    /// module cache when the surface's module-source epoch changes.
    ///
    /// # Errors
    /// Returns [`SharedRuntimeError`] when the session lock, load, blocking bridge, or VM
    /// execution fails.
    pub fn run_compiled_blocking(
        &self,
        chunk: &BytecodeChunk,
        target: &LoadTarget,
        call_options: CallOptions,
    ) -> Result<SharedRuntimeOutcome, SharedRuntimeError> {
        let mut runtime = self.lock_runtime()?;
        let root = runtime
            .load_compiled(chunk, target)
            .map_err(map_retained_load_error)?;
        self.run_loaded_blocking(&mut runtime, &root, call_options)
    }

    /// Runs a prepared graph root on the retained VM and returns owned values.
    ///
    /// This method preserves the source identity, module-source epoch, and runtime capabilities
    /// checked by graph preparation. It unloads the root after execution while retaining the
    /// session VM and module cache.
    ///
    /// # Errors
    /// Returns [`SharedRuntimeError`] when the session lock, prepared context, blocking bridge,
    /// or VM execution fails.
    pub fn run_prepared_blocking(
        &self,
        prepared: &PreparedGraph,
        call_options: CallOptions,
    ) -> Result<SharedRuntimeOutcome, SharedRuntimeError> {
        let mut runtime = self.lock_runtime()?;
        let root = runtime
            .load_prepared(prepared)
            .map_err(SharedRuntimeError::Retained)?;
        self.run_loaded_blocking(&mut runtime, &root, call_options)
    }

    fn run_loaded_blocking(
        &self,
        runtime: &mut Runtime,
        root: &RootHandle,
        call_options: CallOptions,
    ) -> Result<SharedRuntimeOutcome, SharedRuntimeError> {
        let execution_count_before = runtime.execution_count();
        let call_result = self
            .blocking_runtime
            .block_on(runtime.run(root, call_options))
            .map_err(SharedRuntimeError::Blocking);
        let execution_count = runtime
            .execution_count()
            .saturating_sub(execution_count_before);
        let unload_result = runtime.unload(root).or_else(|error| {
            if is_root_invalidated_after_execution(&error, root) {
                // Source invalidation already drained and unloaded this root. The call itself
                // completed, so cleanup must not turn its successful result into a failure.
                Ok(())
            } else {
                Err(error)
            }
        });
        let values =
            call_result?.map_err(|error| map_retained_exec_error(error, execution_count))?;
        unload_result.map_err(SharedRuntimeError::Retained)?;
        Ok(SharedRuntimeOutcome {
            values,
            execution_count,
        })
    }

    fn lock_runtime(&self) -> Result<MutexGuard<'_, Runtime>, SharedRuntimeError> {
        self.runtime
            .lock()
            .map_err(|_| SharedRuntimeError::Poisoned)
    }
}

fn is_root_invalidated_after_execution(error: &LifecycleError, root: &RootHandle) -> bool {
    matches!(
        error,
        LifecycleError::StaleHandle {
            kind: crate::HandleKind::Root,
            handle_epoch,
            current_epoch,
        } if *handle_epoch == root.source_epoch() && current_epoch != handle_epoch
    )
}

fn map_retained_load_error(error: LifecycleError) -> SharedRuntimeError {
    match error {
        LifecycleError::Load(error) => SharedRuntimeError::Load(error),
        other => SharedRuntimeError::Retained(other),
    }
}

fn map_retained_exec_error(error: LifecycleError, execution_count: u64) -> SharedRuntimeError {
    match error {
        LifecycleError::Exec(error) => SharedRuntimeError::Exec {
            error,
            execution_count,
        },
        LifecycleError::Load(error) => SharedRuntimeError::Load(error),
        other => SharedRuntimeError::Retained(other),
    }
}

#[cfg(any())]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use ruau_source::{
        InMemorySource, ModuleId, SourceError, SourceMetadata, SourceResult, SyncSourceProvider,
    };
    use ruau_vm::{
        Cancel, IntoLuaMulti, Limits, ModuleBinding, MultiValue, NativeModule, RuntimeError, Scope,
        ScopedHostFunction,
        module::{Installer as ModuleBuilder, InstallerExt as ModuleBuilderExt},
    };

    use super::*;

    struct HostData(&'static str);

    struct HostValue;

    impl ScopedHostFunction for HostValue {
        fn call<'s>(
            &self,
            scope: &Scope<'s>,
            _args: MultiValue<'s>,
        ) -> Result<MultiValue<'s>, RuntimeError> {
            let value = scope
                .app_data::<HostData>()
                .ok_or_else(|| RuntimeError::runtime("missing host data"))?
                .0;
            value.into_lua_multi(scope)
        }
    }

    struct HostModule;

    impl NativeModule for HostModule {
        fn name(&self) -> &str {
            "host"
        }

        fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
            ruau_declaration::DeclarationSource::Text("declare host: { value: () -> string }")
        }

        fn install(&self, builder: &mut dyn ModuleBuilder) {
            builder.scoped_function("value", ModuleBinding::library("host"), Box::new(HostValue));
        }
    }

    #[derive(Default)]
    struct MutableSource {
        source: Mutex<Vec<u8>>,
        epoch: AtomicU64,
    }

    impl MutableSource {
        fn new(source: impl AsRef<[u8]>) -> Self {
            Self {
                source: Mutex::new(source.as_ref().to_vec()),
                epoch: AtomicU64::new(1),
            }
        }

        fn set_source(&self, source: impl AsRef<[u8]>) {
            *self.source.lock().expect("source mutex") = source.as_ref().to_vec();
            self.epoch.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl SyncSourceProvider for MutableSource {
        fn resolve_sync(
            &self,
            _requester: Option<&ModuleId>,
            request: &[u8],
        ) -> SourceResult<ModuleId> {
            if request == b"dep" {
                return Ok(ModuleId::new("dep"));
            }
            Err(SourceError::MissingModule {
                id: ModuleId::from(request),
            })
        }

        fn read_sync(&self, id: &ModuleId) -> SourceResult<Vec<u8>> {
            if id.as_str() == Some("dep") {
                return Ok(self.source.lock().expect("source mutex").clone());
            }
            Err(SourceError::MissingModule { id: id.clone() })
        }

        fn metadata(&self, id: &ModuleId) -> SourceMetadata {
            SourceMetadata::new(format!("{}.luau", id.to_lossy_string()))
        }

        fn epoch(&self) -> u64 {
            self.epoch.load(Ordering::Relaxed)
        }
    }

    fn test_session(surface: Surface) -> SharedRuntime {
        SharedRuntime::new(
            surface,
            &VmConfig::untrusted(ruau_vm::Ambient::deterministic(0), Limits::unlimited()),
        )
        .expect("session builds")
    }

    fn compile(surface: &Surface, source: &str) -> BytecodeChunk {
        surface
            .compile(
                &ruau_source::Source::text(ruau_source::ModuleId::canonicalized("test"), source),
                &ruau_bytecode::CompileOptions::default(),
            )
            .expect("source compiles")
    }

    fn returned_number(outcome: &SharedRuntimeOutcome) -> f64 {
        match outcome.values.as_slice() {
            [value] => marshaled_number(value),
            values => panic!("expected one number, got {values:?}"),
        }
    }

    fn marshaled_number(value: &ValueSnapshot) -> f64 {
        match value {
            ValueSnapshot::Integer(value) => *value as f64,
            ValueSnapshot::Number(value) => *value,
            value => panic!("expected a number, got {value:?}"),
        }
    }

    #[test]
    fn session_runs_named_chunks_and_returns_raw_values() {
        let session = test_session(Surface::new());
        let chunk = compile(session.surface(), "return 1, 'two'");

        let outcome = session
            .run_compiled_blocking(
                &chunk,
                &LoadTarget::named("session.luau"),
                CallOptions::new(),
            )
            .expect("session run succeeds");

        assert_eq!(marshaled_number(&outcome.values[0]), 1.0);
        assert_eq!(
            outcome.values.get(1),
            Some(&ValueSnapshot::String(b"two".to_vec()))
        );
        assert!(outcome.execution_count > 0);
    }

    #[test]
    fn session_loads_module_ids_for_relative_require() {
        let source =
            Arc::new(InMemorySource::new().with_module(ModuleId::new("app/dep"), "return 41"));
        let surface = Surface::builder()
            .module_source(source)
            .build()
            .expect("surface validates");
        let session = test_session(surface);
        let chunk = compile(session.surface(), "return require('./dep') + 1");

        let outcome = session
            .run_compiled_blocking(
                &chunk,
                &LoadTarget::module_id(ModuleId::new("app/main")),
                CallOptions::new(),
            )
            .expect("module-id run succeeds");

        assert_eq!(returned_number(&outcome), 42.0);
    }

    #[test]
    fn session_runs_prepared_graphs_with_checked_source_identity() {
        let source =
            Arc::new(InMemorySource::new().with_module(ModuleId::new("app/dep"), "return 41"));
        let surface = Surface::builder()
            .module_source(source)
            .build()
            .expect("surface validates");
        let session = test_session(surface);
        let prepared = session
            .surface()
            .prepare_graph_ready(ruau_source::Source::text(
                ModuleId::new("app/main"),
                "return require('./dep') + 1",
            ))
            .expect("graph prepares");

        let outcome = session
            .run_prepared_blocking(&prepared, CallOptions::new())
            .expect("prepared graph run succeeds");

        assert_eq!(returned_number(&outcome), 42.0);
    }

    #[test]
    fn session_rejects_stale_prepared_graphs() {
        let source = Arc::new(MutableSource::new("return 1"));
        let surface = Surface::builder()
            .module_source(source.clone())
            .build()
            .expect("surface validates");
        let session = test_session(surface);
        let prepared = session
            .surface()
            .prepare_graph_ready(ruau_source::Source::text(
                ModuleId::new("main"),
                "return require('dep')",
            ))
            .expect("graph prepares");
        source.set_source("return 2");

        let error = session
            .run_prepared_blocking(&prepared, CallOptions::new())
            .expect_err("stale graph fails");

        assert!(matches!(error, SharedRuntimeError::Retained(_)));
    }

    #[test]
    fn session_keeps_success_when_execution_invalidates_its_source_epoch() {
        let source = Arc::new(MutableSource::new("return 41"));
        let surface = Surface::builder()
            .module_source(source.clone())
            .build()
            .expect("surface validates");
        let session = test_session(surface);
        let prepared = session
            .surface()
            .prepare_graph_ready(ruau_source::Source::text(
                ModuleId::new("main"),
                "local dep = require('dep')\nprint('invalidate')\nreturn dep + 1",
            ))
            .expect("graph prepares");
        let invalidated_source = Arc::clone(&source);
        let options = CallOptions::new().print_sink(Box::new(move |_| {
            invalidated_source.set_source("return 42");
        }));

        let outcome = session
            .run_prepared_blocking(&prepared, options)
            .expect("completed execution remains successful");

        assert_eq!(returned_number(&outcome), 42.0);
        assert!(outcome.execution_count > 0);
    }

    #[test]
    fn session_call_options_carry_app_data_and_print_capture() {
        let surface = Surface::builder()
            .module(Arc::new(HostModule))
            .build()
            .expect("surface validates");
        let session = test_session(surface);
        let chunk = compile(session.surface(), "print('seen')\nreturn host.value()");
        let prints = Arc::new(Mutex::new(Vec::new()));
        let print_capture = Arc::clone(&prints);
        let options = CallOptions::new()
            .app_data(HostData("from-app-data"))
            .print_sink(Box::new(move |line| {
                print_capture
                    .lock()
                    .expect("print mutex")
                    .push(String::from_utf8_lossy(line).trim_end().to_owned());
            }));

        let outcome = session
            .run_compiled_blocking(&chunk, &LoadTarget::named("app-data.luau"), options)
            .expect("session run succeeds");

        assert_eq!(
            outcome.values,
            vec![ValueSnapshot::String(b"from-app-data".to_vec())]
        );
        assert_eq!(*prints.lock().expect("print mutex"), vec!["seen"]);
    }

    #[test]
    fn session_honors_call_cancellation() {
        let session = test_session(Surface::new());
        let chunk = compile(session.surface(), "while true do end");
        let cancel = Cancel::manual();
        cancel.cancel();
        let error = session
            .run_compiled_blocking(
                &chunk,
                &LoadTarget::named("cancelled.luau"),
                CallOptions::new()
                    .cancel(cancel)
                    .limits(Limits::unlimited()),
            )
            .expect_err("cancelled run fails");

        assert!(matches!(
            error,
            SharedRuntimeError::Exec {
                error: ExecError::Stopped(ruau_vm::StopReason::Cancelled),
                ..
            }
        ));
    }

    #[test]
    fn session_clears_module_cache_when_source_epoch_changes() {
        let source = Arc::new(MutableSource::new("return 1"));
        let surface = Surface::builder()
            .module_source(source.clone())
            .build()
            .expect("surface validates");
        let session = test_session(surface);
        let chunk = compile(session.surface(), "return require('dep')");
        let target = LoadTarget::named("main.luau");

        let first = session
            .run_compiled_blocking(&chunk, &target, CallOptions::new())
            .expect("first run succeeds");
        source.set_source("return 2");
        let second = session
            .run_compiled_blocking(&chunk, &target, CallOptions::new())
            .expect("second run succeeds");

        assert_eq!(returned_number(&first), 1.0);
        assert_eq!(returned_number(&second), 2.0);
    }

    #[test]
    fn session_refuses_blocking_inside_current_thread_runtime() {
        let session = test_session(Surface::new());
        let chunk = compile(session.surface(), "return 1");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime builds");

        let error = runtime
            .block_on(async {
                session.run_compiled_blocking(
                    &chunk,
                    &LoadTarget::named("current-thread.luau"),
                    CallOptions::new(),
                )
            })
            .expect_err("current-thread runtime cannot block");

        assert!(matches!(
            error,
            SharedRuntimeError::Blocking(BlockingRuntimeError::AsyncContext)
        ));
    }

    #[test]
    fn session_reuses_cached_blocking_runtime() {
        let session = test_session(Surface::new());
        let chunk = compile(session.surface(), "return 1");
        for _ in 0..2 {
            let outcome = session
                .run_compiled_blocking(
                    &chunk,
                    &LoadTarget::named("cached-runtime.luau"),
                    CallOptions::new(),
                )
                .expect("run succeeds");
            assert_eq!(returned_number(&outcome), 1.0);
        }
    }
}
