use std::{
    error::Error as StdError,
    fmt,
    sync::{Mutex, MutexGuard},
};

use ruau_bytecode::BytecodeChunk;
use ruau_source::ModuleId;
use ruau_surface::{Surface, VmConfig};
use ruau_vm::{CallOptions, ExecError, LoadError, MarshaledValue, Vm, VmBuildError};

use crate::{BlockingRuntime, BlockingRuntimeError};

const SESSION_THREAD_NAME: &str = "ruau-host-surface-session";

/// Retained VM session built from a validated [`Surface`].
pub struct SurfaceSession {
    surface: Surface,
    vm: Mutex<RetainedVm>,
    blocking_runtime: BlockingRuntime,
}

struct RetainedVm {
    vm: Vm,
    source_epoch: u64,
}

/// Load target for one retained session execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionLoadTarget {
    /// Load the chunk with a traceback/debug chunk name.
    Named(Vec<u8>),
    /// Load the chunk as the body for a concrete module id.
    ModuleId(ModuleId),
    /// Load the chunk as a module id with a separate traceback/debug chunk name.
    NamedModule {
        /// Runtime requester identity for relative `require`.
        module_id: ModuleId,
        /// Human-facing chunk name for tracebacks and debug locations.
        chunk_name: Vec<u8>,
    },
}

impl SessionLoadTarget {
    /// Builds a named chunk load target.
    #[must_use]
    pub fn named(chunk_name: impl Into<Vec<u8>>) -> Self {
        Self::Named(chunk_name.into())
    }

    /// Builds a concrete module-id load target.
    #[must_use]
    pub fn module_id(module_id: ModuleId) -> Self {
        Self::ModuleId(module_id)
    }

    /// Builds a module-id load target with a separate chunk name.
    #[must_use]
    pub fn named_module(module_id: ModuleId, chunk_name: impl Into<Vec<u8>>) -> Self {
        Self::NamedModule {
            module_id,
            chunk_name: chunk_name.into(),
        }
    }

    fn load(&self, vm: &mut Vm, chunk: &BytecodeChunk) -> Result<ruau_vm::LoadedModule, LoadError> {
        match self {
            Self::Named(chunk_name) => vm.load_named(chunk, chunk_name),
            Self::ModuleId(module_id) => vm.load_module(chunk, module_id.clone()),
            Self::NamedModule {
                module_id,
                chunk_name,
            } => vm.load_named_module(chunk, module_id.clone(), chunk_name),
        }
    }
}

/// Successful retained session execution.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceSessionOutcome {
    /// Values returned by the executed chunk.
    pub values: Vec<MarshaledValue>,
    /// Host-initiated VM invocations performed by this run.
    pub execution_count: u64,
}

/// Error returned by retained session execution.
#[derive(Debug)]
pub enum SurfaceSessionError {
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
}

impl SurfaceSessionError {
    /// Host-initiated VM invocations completed before this error surfaced.
    #[must_use]
    pub const fn execution_count(&self) -> u64 {
        match self {
            Self::Exec {
                execution_count, ..
            } => *execution_count,
            Self::Poisoned | Self::Blocking(_) | Self::Load(_) => 0,
        }
    }
}

impl fmt::Display for SurfaceSessionError {
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
        }
    }
}

impl StdError for SurfaceSessionError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Poisoned => None,
            Self::Blocking(error) => Some(error),
            Self::Load(error) => Some(error),
            Self::Exec { error, .. } => Some(error),
        }
    }
}

impl SurfaceSession {
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
        let source_epoch = surface_source_epoch(&surface);
        let vm = surface.vm_builder(config).build()?;
        Ok(Self {
            surface,
            vm: Mutex::new(RetainedVm { vm, source_epoch }),
            blocking_runtime,
        })
    }

    /// Returns this session's surface.
    #[must_use]
    pub const fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Runs a compiled chunk on the retained VM and returns owned values.
    ///
    /// This method reloads the chunk for each call, unloads it after execution, and clears the VM
    /// module cache when the surface's module-source epoch changes.
    ///
    /// # Errors
    /// Returns [`SurfaceSessionError`] when the session lock, load, blocking bridge, or VM
    /// execution fails.
    pub fn run_compiled_blocking(
        &self,
        chunk: &BytecodeChunk,
        target: &SessionLoadTarget,
        call_options: CallOptions,
    ) -> Result<SurfaceSessionOutcome, SurfaceSessionError> {
        let mut retained = self.lock_vm()?;
        self.refresh_source_epoch(&mut retained);
        let loaded = target
            .load(&mut retained.vm, chunk)
            .map_err(SurfaceSessionError::Load)?;
        let execution_count_before = retained.vm.execution_count();
        let call_result = self
            .blocking_runtime
            .block_on(retained.vm.exec_async(&loaded, call_options))
            .map_err(SurfaceSessionError::Blocking);
        let execution_count = retained
            .vm
            .execution_count()
            .saturating_sub(execution_count_before);
        retained.vm.unload(loaded);
        let values = call_result?.map_err(|error| SurfaceSessionError::Exec {
            error,
            execution_count,
        })?;
        Ok(SurfaceSessionOutcome {
            values,
            execution_count,
        })
    }

    fn lock_vm(&self) -> Result<MutexGuard<'_, RetainedVm>, SurfaceSessionError> {
        self.vm.lock().map_err(|_| SurfaceSessionError::Poisoned)
    }

    fn refresh_source_epoch(&self, retained: &mut RetainedVm) {
        let source_epoch = surface_source_epoch(&self.surface);
        if retained.source_epoch != source_epoch {
            retained.vm.clear_module_cache();
            retained.source_epoch = source_epoch;
        }
    }
}

fn surface_source_epoch(surface: &Surface) -> u64 {
    surface.module_source().map_or(0, |source| source.epoch())
}

#[cfg(any())]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use ruau_source::{
        InMemorySource, ModuleSourceError, ModuleSourceResult, SourceMetadata, SyncModuleSource,
    };
    use ruau_vm::{
        Cancel, IntoLuaMulti, Limits, ModuleBuilderExt, MultiValue, RuntimeError, Scope,
        ScopedHostFunction,
    };
    use ruau_vm_api::{ModuleBinding, ModuleBuilder, NativeModule};

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

        fn declaration(&self) -> ruau_decl::DeclSource<'_> {
            ruau_decl::DeclSource::Text("declare host: { value: () -> string }")
        }

        fn build(&self, builder: &mut dyn ModuleBuilder) {
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

    impl SyncModuleSource for MutableSource {
        fn resolve_sync(
            &self,
            _requester: Option<&ModuleId>,
            request: &[u8],
        ) -> ModuleSourceResult<ModuleId> {
            if request == b"dep" {
                return Ok(ModuleId::new("dep"));
            }
            Err(ModuleSourceError::MissingModule {
                id: ModuleId::from(request),
            })
        }

        fn read_sync(&self, id: &ModuleId) -> ModuleSourceResult<Vec<u8>> {
            if id.as_str() == Some("dep") {
                return Ok(self.source.lock().expect("source mutex").clone());
            }
            Err(ModuleSourceError::MissingModule { id: id.clone() })
        }

        fn metadata(&self, id: &ModuleId) -> SourceMetadata {
            SourceMetadata::new(format!("{}.luau", id.to_lossy_string()))
        }

        fn epoch(&self) -> u64 {
            self.epoch.load(Ordering::Relaxed)
        }
    }

    fn test_session(surface: Surface) -> SurfaceSession {
        SurfaceSession::new(surface, &VmConfig::deterministic(0)).expect("session builds")
    }

    fn compile(surface: &Surface, source: &str) -> BytecodeChunk {
        surface
            .compile_bytes(source.as_bytes())
            .expect("source compiles")
    }

    fn returned_number(outcome: &SurfaceSessionOutcome) -> f64 {
        match outcome.values.as_slice() {
            [value] => marshaled_number(value),
            values => panic!("expected one number, got {values:?}"),
        }
    }

    fn marshaled_number(value: &MarshaledValue) -> f64 {
        match value {
            MarshaledValue::Integer(value) => *value as f64,
            MarshaledValue::Number(value) => *value,
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
                &SessionLoadTarget::named("session.luau"),
                CallOptions::new(),
            )
            .expect("session run succeeds");

        assert_eq!(marshaled_number(&outcome.values[0]), 1.0);
        assert_eq!(
            outcome.values.get(1),
            Some(&MarshaledValue::String(b"two".to_vec()))
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
                &SessionLoadTarget::module_id(ModuleId::new("app/main")),
                CallOptions::new(),
            )
            .expect("module-id run succeeds");

        assert_eq!(returned_number(&outcome), 42.0);
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
            .run_compiled_blocking(&chunk, &SessionLoadTarget::named("app-data.luau"), options)
            .expect("session run succeeds");

        assert_eq!(
            outcome.values,
            vec![MarshaledValue::String(b"from-app-data".to_vec())]
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
                &SessionLoadTarget::named("cancelled.luau"),
                CallOptions::new()
                    .cancel(cancel)
                    .limits(Limits::unlimited()),
            )
            .expect_err("cancelled run fails");

        assert!(matches!(
            error,
            SurfaceSessionError::Exec {
                error: ExecError::Cancelled,
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
        let target = SessionLoadTarget::named("main.luau");

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
                    &SessionLoadTarget::named("current-thread.luau"),
                    CallOptions::new(),
                )
            })
            .expect_err("current-thread runtime cannot block");

        assert!(matches!(
            error,
            SurfaceSessionError::Blocking(BlockingRuntimeError::AsyncContext)
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
                    &SessionLoadTarget::named("cached-runtime.luau"),
                    CallOptions::new(),
                )
                .expect("run succeeds");
            assert_eq!(returned_number(&outcome), 1.0);
        }
    }
}
