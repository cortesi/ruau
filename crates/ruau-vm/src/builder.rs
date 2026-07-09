use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use ruau_source::ModuleSource;
use ruau_vm_api::NativeModule;

use crate::{
    PrintSink, Vm,
    heap::Heap,
    host_type::{self, HostType},
    install_base_globals,
    limits::{Ambient, Limits, SinkQuota},
    load::{CompiledModule, LoadError},
    next_heap_id,
    registry::{Environment, ModuleInstallError},
    runtime_capabilities::{Library, RuntimeCapabilities},
    runtime_compile::{self, RuntimeCompiler},
    sandbox::SandboxError,
    scope,
    snapshot::{self, SnapshotError},
    state::Thread,
};

/// VM sandboxing policy applied by [`VmBuilder::build`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmSandboxPolicy {
    /// Build a trusted host VM without installing the untrusted-code sandbox.
    TrustedHost,
    /// Build a VM for untrusted code and install the sandbox before returning.
    Untrusted,
}

/// Why building a [`Vm`] failed.
///
/// `ambient`, `limits`, runtime capabilities, and a sandbox policy are
/// required, and native modules must install cleanly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmBuildError {
    /// No ambient mode ([`VmBuilder::ambient`]) was selected.
    MissingAmbient,
    /// No resource ceilings ([`VmBuilder::limits`]) were selected.
    MissingLimits,
    /// No runtime capabilities ([`VmBuilder::runtime_capabilities`]) were selected.
    MissingRuntimeCapabilities,
    /// No sandbox policy ([`VmBuilder::sandboxed`] or
    /// [`VmBuilder::trusted_host`]) was selected.
    MissingSandboxPolicy,
    /// A native module binding failed to install.
    ModuleInstall(ModuleInstallError),
    /// A [`VmBuilder::preload`] artifact failed to instantiate.
    Preload(LoadError),
    /// The VM built, but sandboxing failed; the VM is discarded.
    Sandbox(SandboxError),
}

impl std::fmt::Display for VmBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let field = match self {
            Self::MissingAmbient => "ambient",
            Self::MissingLimits => "limits",
            Self::MissingRuntimeCapabilities => "runtime_capabilities",
            Self::MissingSandboxPolicy => {
                return write!(
                    f,
                    "VM builder is missing a sandbox policy: call `sandboxed()` or `trusted_host()`"
                );
            }
            Self::ModuleInstall(error) => {
                return write!(f, "VM build failed installing a native module: {error}");
            }
            Self::Preload(error) => {
                return write!(
                    f,
                    "VM build failed instantiating a preload artifact: {error}"
                );
            }
            Self::Sandbox(error) => {
                return write!(f, "VM build failed installing the sandbox: {error}");
            }
        };
        write!(
            f,
            "VM builder is missing the required `{field}` configuration"
        )
    }
}

impl std::error::Error for VmBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ModuleInstall(error) => Some(error),
            Self::Preload(error) => Some(error),
            Self::Sandbox(error) => Some(error),
            Self::MissingAmbient
            | Self::MissingLimits
            | Self::MissingRuntimeCapabilities
            | Self::MissingSandboxPolicy => None,
        }
    }
}

/// Builder for a [`Vm`].
#[derive(Default)]
pub struct VmBuilder {
    ambient: Option<Ambient>,
    limits: Option<Limits>,
    environment: Option<Environment>,
    runtime_capabilities: Option<RuntimeCapabilities>,
    runtime_compiler: Option<std::sync::Arc<dyn RuntimeCompiler>>,
    module_source: Option<std::sync::Arc<dyn ModuleSource>>,
    print_sink: Option<PrintSink>,
    app_data: scope::AppData,
    host_types: Vec<std::sync::Arc<host_type::HostType>>,
    preloads: Vec<CompiledModule>,
    sandbox_policy: Option<VmSandboxPolicy>,
}

impl VmBuilder {
    /// Sets the ambient mode. Required: [`build`](VmBuilder::build) fails with
    /// [`VmBuildError::MissingAmbient`] if it is not set.
    #[must_use]
    pub fn ambient(mut self, ambient: Ambient) -> Self {
        self.ambient = Some(ambient);
        self
    }

    /// Sets the resource ceilings.
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Selects which standard libraries and base capabilities to install. Required:
    /// [`build`](VmBuilder::build) fails with
    /// [`VmBuildError::MissingRuntimeCapabilities`] if it is not set.
    #[must_use]
    pub fn runtime_capabilities(mut self, capabilities: RuntimeCapabilities) -> Self {
        self.runtime_capabilities = Some(capabilities);
        self
    }

    /// Installs the compiler used by runtime source compilation (`loadstring`).
    #[must_use]
    pub fn runtime_compiler(mut self, compiler: std::sync::Arc<dyn RuntimeCompiler>) -> Self {
        self.runtime_compiler = Some(compiler);
        self
    }

    /// Installs the source provider for `require`. Supplying one installs the
    /// `require` global; without it, `require` is absent (an embedder opts in).
    #[must_use]
    pub fn module_source(mut self, source: std::sync::Arc<dyn ModuleSource>) -> Self {
        self.module_source = Some(source);
        self
    }

    /// Installs the VM-level default `print` sink.
    ///
    /// Per-call options may replace it for one invocation; the default is
    /// restored when that call returns.
    #[must_use]
    pub fn print_sink(mut self, sink: PrintSink) -> Self {
        self.print_sink = Some(sink);
        self
    }

    /// Installs the VM-level default `print` sink with quota accounting.
    #[must_use]
    pub fn print_sink_with_quota(mut self, sink: PrintSink, quota: SinkQuota) -> Self {
        self.print_sink = Some(quota.apply(sink));
        self
    }

    /// Installs VM-level default typed app data.
    ///
    /// One value per Rust type; later calls with the same type replace the
    /// previous value.
    #[must_use]
    pub fn app_data<T: std::any::Any + Send + Sync>(mut self, value: T) -> Self {
        self.app_data.set(value);
        self
    }

    /// Registers a native module to install during [`build`](Self::build).
    #[must_use]
    pub fn module(mut self, module: std::sync::Arc<dyn NativeModule>) -> Self {
        let mut environment = self.environment.take().unwrap_or_default();
        environment.register(module);
        self.environment = Some(environment);
        self
    }

    /// Registers a host userdata type.
    ///
    /// Instances are created inside scope steps with
    /// [`Scope::create_userdata`](scope::Scope::create_userdata).
    #[must_use]
    pub fn host_type(mut self, host_type: HostType) -> Self {
        self.host_types.push(std::sync::Arc::new(host_type));
        self
    }

    /// Queues a compiled artifact to instantiate during [`build`](Self::build).
    ///
    /// Artifacts load in registration order. Retrieve loaded modules with
    /// [`Vm::take_preloaded`]. A mismatch or load failure returns
    /// [`VmBuildError::Preload`].
    #[must_use]
    pub fn preload(mut self, module: &CompiledModule) -> Self {
        self.preloads.push(module.clone());
        self
    }

    /// Selects the untrusted-code policy: [`build`](Self::build) installs the
    /// sandbox before returning the VM.
    ///
    /// A sandbox policy is required: `build` fails with
    /// [`VmBuildError::MissingSandboxPolicy`] when neither this nor
    /// [`trusted_host`](Self::trusted_host) was called.
    #[must_use]
    pub fn sandboxed(mut self) -> Self {
        self.sandbox_policy = Some(VmSandboxPolicy::Untrusted);
        self
    }

    /// Selects the trusted-host policy: [`build`](Self::build) does not
    /// install the untrusted-code sandbox.
    ///
    /// A trusted VM may later be sealed with [`Vm::sandbox_for_untrusted`] —
    /// the build-then-seal flow: build trusted, run host setup chunks, then
    /// seal before any untrusted script runs.
    ///
    /// A sandbox policy is required: `build` fails with
    /// [`VmBuildError::MissingSandboxPolicy`] when neither this nor
    /// [`sandboxed`](Self::sandboxed) was called.
    #[must_use]
    pub fn trusted_host(mut self) -> Self {
        self.sandbox_policy = Some(VmSandboxPolicy::TrustedHost);
        self
    }

    /// Builds the VM.
    ///
    /// # Errors
    /// Returns a [`VmBuildError`] when `ambient`, `limits`, runtime
    /// capabilities, or the sandbox policy ([`sandboxed`](Self::sandboxed) /
    /// [`trusted_host`](Self::trusted_host)) were not set, or when sandbox
    /// installation fails under [`VmSandboxPolicy::Untrusted`].
    pub fn build(self) -> Result<Vm, VmBuildError> {
        self.build_with_sandbox_timing().map(|(vm, _)| vm)
    }

    /// Builds the VM, returning the time spent installing the sandbox.
    ///
    /// This is exposed for runner instrumentation; ordinary callers should use
    /// [`build`](Self::build).
    #[doc(hidden)]
    pub fn build_with_sandbox_timing(self) -> Result<(Vm, Duration), VmBuildError> {
        let ambient = self.ambient.ok_or(VmBuildError::MissingAmbient)?;
        let limits = self.limits.ok_or(VmBuildError::MissingLimits)?;
        let runtime_capabilities = self
            .runtime_capabilities
            .ok_or(VmBuildError::MissingRuntimeCapabilities)?;
        let sandbox_policy = self
            .sandbox_policy
            .ok_or(VmBuildError::MissingSandboxPolicy)?;
        // A process-unique heap nonce, decoupled from the (determinism) hash seed
        // so two same-seed VMs reject each other's handles (§6.2). The nonce only
        // tags handles for cross-VM rejection; it never enters a computation, so a
        // monotonic counter is sound even under the deterministic seam.
        let id = next_heap_id();
        let mut heap = Heap::new(id, ambient.config);
        if let Some(print_sink) = self.print_sink {
            heap.set_print_sink(print_sink);
        }
        if let Some(runtime_compiler) = self.runtime_compiler {
            heap.set_runtime_compiler(runtime_compiler);
        } else {
            // The VM-local fallback compiler applies the capability selector's
            // compiler-half
            // restriction (the `RuntimeCapabilities::compile_source` rule), so a disabled
            // library's constants are never folded into a runtime-compiled
            // chunk.
            heap.set_runtime_compiler(std::sync::Arc::new(
                runtime_compile::VmRuntimeCompiler::for_runtime_capabilities(&runtime_capabilities),
            ));
        }
        // Recorded on the heap so the borrowed-scope runtime-compilation entry
        // points (`Scope::load_chunk`/`Scope::eval_chunk`) can enforce the
        // capability gate at call time; `loadstring` is gated at install below.
        heap.set_runtime_compilation_enabled(runtime_capabilities.runtime_compilation_enabled());
        // Set before installing globals: `require` is installed only when a source
        // is present, so the gate in `install_base_globals` reads it here.
        if let Some(module_source) = self.module_source {
            heap.set_module_source(module_source);
        }
        let environment = self.environment.unwrap_or_default();
        if environment.has_require_exports() {
            heap.enable_native_require();
        }
        // The live main thread is an arena object (so the collector can trace it),
        // built then allocated; `alloc_thread` attaches its register stack to the
        // heap meter. Its own handle anchors its open upvalues.
        let globals_table = install_base_globals(&mut heap, &runtime_capabilities);
        let mut main = Thread::new();
        main.globals = globals_table;
        let main_thread = heap
            .alloc_thread(main)
            .expect("a fresh heap has room for the main thread");
        if let Some(thread) = heap.thread_mut(main_thread) {
            thread.id = Some(main_thread);
        }
        // Install the selected native modules on top of the fixed builtin surface.
        // Modules are host-supplied (an untrusted script cannot register one), so
        // this build-time setup is trusted and runs before the resource ceilings
        // are armed. An install failure — an accidental builtin collision, an
        // override with no target, a bad payload, or a mid-install allocation
        // failure — leaves a partial surface, so the build fails closed: no VM
        // is handed back to run a script against a half-installed environment.
        let (install_error, named_bindings, mut module_host_types, support_chunks) =
            match globals_table {
                Some(table) => match environment.install(&mut heap, table) {
                    Ok(installed) => (
                        None,
                        installed.named_bindings,
                        installed.host_types,
                        installed.support_chunks,
                    ),
                    Err(error) => return Err(VmBuildError::ModuleInstall(error)),
                },
                None => (
                    Some("VM has no global table for native module installation".to_owned()),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ),
            };
        // Host userdata types install after modules, still pre-sandbox and
        // trusted (host-supplied only). A failed install — duplicate
        // registration or allocation failure — leaves a partial dispatch
        // surface, so it poisons the VM and the first entry errors cleanly.
        let mut install_error = install_error;
        if install_error.is_none() {
            module_host_types.extend(self.host_types);
            if let Err(error) = host_type::install_host_types(&mut heap, &module_host_types) {
                install_error = Some(error);
            }
        }
        heap.set_clock(ambient.mode);
        // `environment` is installed into the heap above and not retained:
        // every host closure and library table now lives in heap objects.
        drop(environment);
        let mut vm = Vm {
            heap,
            execution_count: 0,
            main_thread,
            ambient,
            limits,
            runtime_capabilities: runtime_capabilities.clone(),
            poisoned: install_error.is_some(),
            poison_reason: install_error,
            named_bindings,
            app_data: std::cell::RefCell::new(self.app_data),
            preloaded: Vec::new(),
        };
        // The trusted build-time Lua prelude (Lua-level stdlib that cannot be an
        // engine builtin) runs before the resource ceilings are armed, like module
        // install; a failure poisons the VM so the first `call` errors cleanly. It
        // defines `coroutine.wrap` over `coroutine.*` and `table.pack`/`unpack`, so
        // it runs only when the capabilities provide both libraries.
        let prelude_libraries = runtime_capabilities.includes(Library::Coroutine)
            && runtime_capabilities.includes(Library::Table);
        if !vm.poisoned && prelude_libraries && !vm.run_prelude() {
            vm.poisoned = true;
        }
        if !vm.poisoned
            && let Err(error) = vm.run_support_chunks(&support_chunks)
        {
            vm.poisoned = true;
            vm.poison_reason = Some(error);
        }
        vm.apply_default_limits();
        // Instantiate preload artifacts last, against the fully-installed
        // surface, exactly as a post-build `load_compiled` would. Fail closed:
        // a VM missing a requested module is never handed back.
        for artifact in &self.preloads {
            let module = vm.load_compiled(artifact).map_err(VmBuildError::Preload)?;
            vm.preloaded.push(module);
        }
        let sandbox_time = match sandbox_policy {
            VmSandboxPolicy::TrustedHost => Duration::ZERO,
            VmSandboxPolicy::Untrusted => {
                sandbox_for_untrusted_with_timing(&mut vm).map_err(VmBuildError::Sandbox)?
            }
        };
        Ok((vm, sandbox_time))
    }

    /// Builds this template and restores a compatible VM snapshot into it.
    ///
    /// # Errors
    /// Returns [`SnapshotError`] when the template build fails, bytes are
    /// malformed, stamps differ, or the decoded heap image is invalid.
    pub fn restore_snapshot(self, snapshot: impl AsRef<[u8]>) -> Result<Vm, SnapshotError> {
        let vm = self.trusted_host().build().map_err(SnapshotError::Build)?;
        snapshot::restore_snapshot_bytes(vm, snapshot.as_ref())
    }
}

fn sandbox_for_untrusted_with_timing(vm: &mut Vm) -> Result<Duration, SandboxError> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let started = Instant::now();
        vm.sandbox_for_untrusted()?;
        Ok(started.elapsed())
    }

    #[cfg(target_arch = "wasm32")]
    {
        vm.sandbox_for_untrusted()?;
        Ok(Duration::ZERO)
    }
}

#[cfg(any())]
impl VmBuilder {
    /// Builds for a test, supplying the historical `build()` defaults — a
    /// `deterministic(0)` ambient, default (unbounded) limits, the default
    /// runtime capabilities, and the trusted-host sandbox policy — for any of
    /// `ambient`/`limits`/`runtime_capabilities`/sandbox policy the test did
    /// not set, and panicking on a build error. Lets a test exercise one field
    /// (`Vm::builder().runtime_capabilities(..).build_for_test()`) without
    /// re-stating the others now that [`VmBuilder::build`] fails closed.
    pub(crate) fn build_for_test(mut self) -> Vm {
        self.ambient
            .get_or_insert_with(|| Ambient::deterministic(0));
        self.limits.get_or_insert_with(Limits::unlimited);
        self.runtime_capabilities
            .get_or_insert_with(RuntimeCapabilities::default);
        self.sandbox_policy
            .get_or_insert(VmSandboxPolicy::TrustedHost);
        self.build().expect("test vm builds")
    }
}

/// A default VM for tests.
#[cfg(any())]
pub fn test_vm() -> Vm {
    Vm::builder().build_for_test()
}
