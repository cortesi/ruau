//! Runtime source compilation support for `loadstring` and source-backed `require`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use ruau_bytecode::{
    BytecodeChunk, CompileErrorKind, UpstreamCompilerOptions, chunkify_parse_error,
    compile_source_bytes_strict_with_upstream_options, encode_chunk,
};

use crate::{cancel::Cancel, limits::EffectiveLimits};

/// Concrete limits passed to a runtime compiler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeCompileLimits {
    /// Maximum source bytes accepted by one runtime compilation.
    pub max_source_bytes: usize,
    /// Maximum instruction words produced by one runtime compilation.
    pub max_compiled_instructions: usize,
    /// Maximum encoded bytecode bytes produced by one runtime compilation.
    pub max_compiled_bytecode_bytes: usize,
}

impl RuntimeCompileLimits {
    #[must_use]
    pub(crate) fn from_effective(limits: EffectiveLimits) -> Self {
        Self {
            max_source_bytes: limits.max_runtime_compile_source_bytes,
            max_compiled_instructions: limits.max_runtime_compile_instructions,
            max_compiled_bytecode_bytes: limits.max_runtime_compile_bytecode_bytes,
        }
    }
}

/// Runtime compilation governance passed to the compiler hook.
#[derive(Clone, Debug)]
pub struct RuntimeCompileContext {
    /// Product and source limits for this runtime compilation.
    pub limits: RuntimeCompileLimits,
    /// Cancellation signal for this runtime compilation.
    pub cancel: Option<Cancel>,
    /// Canonical module id when compiling a `require`d module. `None` for
    /// `loadstring` and other anonymous runtime compilation.
    pub module_id: Option<crate::ModuleId>,
}

impl RuntimeCompileContext {
    #[must_use]
    pub(crate) fn new(limits: RuntimeCompileLimits, cancel: Option<Cancel>) -> Self {
        Self {
            limits,
            cancel,
            module_id: None,
        }
    }

    #[must_use]
    pub(crate) fn with_module_id(mut self, module_id: crate::ModuleId) -> Self {
        self.module_id = Some(module_id);
        self
    }

    /// Returns a loadstring-shaped error when the request has been cancelled.
    ///
    /// Compilers should check before starting work and between expensive stages.
    pub fn check_cancelled(&self) -> Result<(), Vec<u8>> {
        if self.cancel.as_ref().is_some_and(Cancel::is_cancelled) {
            return Err(b"runtime compilation cancelled".to_vec());
        }
        Ok(())
    }
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
            return Err(runtime_compile_cancelled());
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

/// Compiles a `loadstring` source payload into VM bytecode.
///
/// Implementations return raw diagnostic bytes without a chunk prefix; the
/// builtin wraps the message in `loadstring`'s `(nil, message)` shape.
pub trait RuntimeCompiler: Send + Sync {
    /// Compiles `source` under `limits`.
    ///
    /// # Errors
    /// Returns diagnostic bytes suitable for `loadstring`'s second return value.
    fn compile(
        &self,
        source: &[u8],
        context: RuntimeCompileContext,
    ) -> Result<BytecodeChunk, Vec<u8>>;
}

/// VM-local compiler used when no custom [`RuntimeCompiler`] is installed.
#[derive(Default)]
pub struct VmRuntimeCompiler {
    /// Globals the VM's runtime capabilities omit, marked mutable for every
    /// compilation - the compiler half of
    /// [`RuntimeCapabilities::compile_source`](crate::RuntimeCapabilities::compile_source).
    /// A non-default global is
    /// neither constant-folded nor FASTCALLed, so a runtime-compiled chunk
    /// cannot recover a disabled library's constants by escalating the
    /// optimization level with a `--!optimize 2` hot comment; the reference
    /// resolves to the absent runtime global and fails closed.
    suppressed_globals: Vec<String>,
}

impl VmRuntimeCompiler {
    /// A VM-local compiler applying capability fold/FASTCALL suppression to
    /// every runtime compilation.
    #[must_use]
    pub(crate) fn for_runtime_capabilities(capabilities: &crate::RuntimeCapabilities) -> Self {
        Self {
            suppressed_globals: capabilities
                .omitted_libraries()
                .map(|library| library.global_name().to_owned())
                .collect(),
        }
    }
}

impl RuntimeCompiler for VmRuntimeCompiler {
    fn compile(
        &self,
        source: &[u8],
        context: RuntimeCompileContext,
    ) -> Result<BytecodeChunk, Vec<u8>> {
        let cancellation = RuntimeCompileCancellation::new(context.cancel.clone());
        cancellation.check_cancelled()?;
        let limits = context.limits;
        enforce_runtime_compile_limit("source byte", source.len(), limits.max_source_bytes)?;

        let mut options = UpstreamCompilerOptions::for_vm_execution();
        options
            .mutable_globals
            .extend(self.suppressed_globals.iter().cloned());
        // Compile byte-preservingly: a `loadstring` argument is an arbitrary
        // byte string, and a lossy UTF-8 view would corrupt non-ASCII bytes in
        // string literals (shifting `string.find` offsets, etc.).
        let chunk = match chunkify_parse_error(compile_source_bytes_strict_with_upstream_options(
            source,
            &options,
            Some(cancellation.flag()),
        )) {
            Ok(valid @ BytecodeChunk::Valid { .. }) => valid,
            Ok(BytecodeChunk::Error { message }) => return Err(message),
            Err(error) if error.kind() == CompileErrorKind::Cancelled => {
                return Err(runtime_compile_cancelled());
            }
            Err(error) => return Err(error.to_string().into_bytes()),
        };
        cancellation.check_cancelled()?;

        let metrics = runtime_compile_metrics(&chunk)?;
        enforce_runtime_compile_limit(
            "compiled instruction",
            metrics.instructions,
            limits.max_compiled_instructions,
        )?;
        enforce_runtime_compile_limit(
            "compiled bytecode byte",
            metrics.encoded_bytes,
            limits.max_compiled_bytecode_bytes,
        )?;
        Ok(chunk)
    }
}

fn runtime_compile_cancelled() -> Vec<u8> {
    b"runtime compilation cancelled".to_vec()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RuntimeCompileMetrics {
    instructions: usize,
    encoded_bytes: usize,
}

fn runtime_compile_metrics(chunk: &BytecodeChunk) -> Result<RuntimeCompileMetrics, Vec<u8>> {
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
    let encoded_bytes = encode_chunk(chunk)
        .map_err(|error| {
            format!("runtime compilation product failed to encode: {error}").into_bytes()
        })?
        .len();
    Ok(RuntimeCompileMetrics {
        instructions,
        encoded_bytes,
    })
}

/// Enforces a runtime compilation product limit.
pub fn enforce_runtime_compile_limit(label: &str, used: usize, cap: usize) -> Result<(), Vec<u8>> {
    if used > cap {
        return Err(
            format!("runtime compilation {label} limit exceeded: {used} > {cap}").into_bytes(),
        );
    }
    Ok(())
}
