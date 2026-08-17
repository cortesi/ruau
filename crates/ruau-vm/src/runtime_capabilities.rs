//! VM standard-library and runtime capability selection.
//!
//! [`RuntimeCapabilities`] selects optional library tables (`math`, `os`,
//! `buffer`, ...) and capabilities such as runtime compilation.

use std::sync::{Arc, atomic::AtomicBool};

use ruau_bytecode::{BytecodeChunk, CompileError, CompileOptions, UpstreamCompilerOptions};
use ruau_syntax::parse::ParsedModule;

/// An optional standard library a [`RuntimeCapabilities`] value can include or omit.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Library {
    /// `coroutine` - create/resume/yield/status/wrap/...
    Coroutine,
    /// `string` - patterns, `format`, `pack`, plus the string metatable.
    String,
    /// `math` - the numeric surface and its constants.
    Math,
    /// `integer` - 64-bit integer construction and bit/arithmetic helpers.
    Integer,
    /// `table` - insert/remove/sort/pack/move/freeze/...
    Table,
    /// `bit32` - 32-bit bitwise operations.
    Bit32,
    /// `utf8` - codepoint iteration and `charpattern`.
    Utf8,
    /// `os` - the time-only surface (no filesystem or process access).
    Os,
    /// `buffer` - fixed-size little-endian byte buffers.
    Buffer,
    /// `vector` - 3-component float vectors and their constants.
    Vector,
    /// `debug` - script-visible `debug.info` and `debug.traceback`.
    Debug,
}

impl Library {
    /// Every optional library, in install order.
    pub const ALL: [Self; 11] = [
        Self::Coroutine,
        Self::String,
        Self::Math,
        Self::Integer,
        Self::Table,
        Self::Bit32,
        Self::Utf8,
        Self::Os,
        Self::Buffer,
        Self::Vector,
        Self::Debug,
    ];

    /// The global name the library installs under (`math`, `os`, ...), as the
    /// raw bytes the VM interns.
    #[must_use]
    pub fn global_name_bytes(self) -> &'static [u8] {
        match self {
            Self::Coroutine => b"coroutine",
            Self::String => b"string",
            Self::Math => b"math",
            Self::Integer => b"integer",
            Self::Table => b"table",
            Self::Bit32 => b"bit32",
            Self::Utf8 => b"utf8",
            Self::Os => b"os",
            Self::Buffer => b"buffer",
            Self::Vector => b"vector",
            Self::Debug => b"debug",
        }
    }

    /// The global name as a string; the bytes are ASCII.
    #[must_use]
    pub fn global_name(self) -> &'static str {
        std::str::from_utf8(self.global_name_bytes()).expect("library names are ASCII")
    }

    fn snapshot_bit(self) -> u16 {
        1 << (self as u16)
    }
}

/// Selects which standard libraries and base-surface capabilities a VM installs.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct RuntimeCapabilities {
    libraries: Vec<Library>,
    runtime_compilation: bool,
}

impl RuntimeCapabilities {
    /// Selects exactly the supplied standard libraries.
    ///
    /// Duplicates are ignored and the stored identity is canonicalized to
    /// [`Library::ALL`] order, so equality, artifact stamps, and snapshot stamps
    /// are independent of input order.
    #[must_use]
    pub fn from_libraries<I>(libraries: I) -> Self
    where
        I: IntoIterator<Item = Library>,
    {
        Self {
            libraries: canonical_libraries(libraries),
            runtime_compilation: false,
        }
    }

    /// The selected standard libraries, in canonical install order.
    #[must_use]
    pub fn libraries(&self) -> &[Library] {
        &self.libraries
    }

    /// Whether `library` is installed under these capabilities.
    #[must_use]
    pub fn includes(&self, library: Library) -> bool {
        self.libraries.contains(&library)
    }

    /// Whether runtime compilation through `loadstring` is installed.
    #[must_use]
    pub const fn runtime_compilation_enabled(&self) -> bool {
        self.runtime_compilation
    }

    /// Enables runtime compilation through `loadstring`.
    #[must_use]
    pub fn enable_runtime_compilation(mut self) -> Self {
        self.runtime_compilation = true;
        self
    }

    /// The libraries these capabilities omit - the complement of what they
    /// install. The compiler and type-checker surfaces use these to drop a
    /// disabled library's global from optimization and from name resolution.
    pub fn omitted_libraries(&self) -> impl Iterator<Item = Library> + '_ {
        Library::ALL
            .into_iter()
            .filter(move |&library| !self.includes(library))
    }

    fn restrict_compile_options(&self, options: &mut UpstreamCompilerOptions) {
        options.mutable_globals.extend(
            self.omitted_libraries()
                .map(|library| library.global_name().to_owned()),
        );
    }

    /// Compiles `source` for these VM capabilities.
    ///
    /// Omitted libraries are added to `mutable_globals`, preventing constant
    /// folding and FASTCALL emission for globals the runtime will not install.
    /// The source is accepted as raw bytes, preserving non-UTF-8 source.
    ///
    /// # Errors
    /// Returns [`CompileError`] for malformed source or compiler limits.
    pub fn compile_source(
        &self,
        source: &[u8],
        base: &CompileOptions,
    ) -> Result<BytecodeChunk, CompileError> {
        self.compile_source_with_cancel(source, base, None)
    }

    /// Cancellation-aware form of [`compile_source`](Self::compile_source).
    ///
    /// # Errors
    /// As [`compile_source`](Self::compile_source), plus cancellation when the
    /// flag is set at a cooperative compiler safepoint.
    pub fn compile_source_with_cancel(
        &self,
        source: &[u8],
        base: &CompileOptions,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<BytecodeChunk, CompileError> {
        let base = base.to_upstream_options();
        self.compile_source_with_upstream_options_and_cancel(source, &base, cancel)
    }

    /// Compiles an existing shared parse product for these VM capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`CompileError`] for malformed source, incompatible parser
    /// options, compiler limits, or cancellation.
    #[doc(hidden)]
    pub fn compile_parsed_module_with_cancel(
        &self,
        parsed: &ParsedModule,
        base: &CompileOptions,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<BytecodeChunk, CompileError> {
        let mut options = base.to_upstream_options();
        self.restrict_compile_options(&mut options);
        options.clear_dead_stack_slots = true;
        ruau_bytecode::compile_parsed_module_strict_with_upstream_options(parsed, &options, cancel)
    }

    /// Compiles `source` with the repository's upstream-fixture option shape.
    #[doc(hidden)]
    pub fn compile_source_with_upstream_options(
        &self,
        source: &[u8],
        base: &UpstreamCompilerOptions,
    ) -> Result<BytecodeChunk, CompileError> {
        self.compile_source_with_upstream_options_and_cancel(source, base, None)
    }

    /// Cancellation-aware form of [`compile_source_with_upstream_options`](Self::compile_source_with_upstream_options).
    #[doc(hidden)]
    pub fn compile_source_with_upstream_options_and_cancel(
        &self,
        source: &[u8],
        base: &UpstreamCompilerOptions,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<BytecodeChunk, CompileError> {
        let mut options = base.clone();
        self.restrict_compile_options(&mut options);
        options.clear_dead_stack_slots = true;
        match std::str::from_utf8(source) {
            Ok(text) => {
                ruau_bytecode::compile_source_strict_with_upstream_options(text, &options, cancel)
            }
            Err(_) => ruau_bytecode::compile_source_bytes_strict_with_upstream_options(
                source, &options, cancel,
            ),
        }
    }

    /// Compiles and validates `source` into a [`CompiledModule`](crate::CompiledModule).
    ///
    /// Load the artifact into VMs with matching runtime capabilities using
    /// [`Vm::load_compiled`](crate::Vm::load_compiled) or
    /// [`VmBuilder::preload`](crate::VmBuilder::preload).
    ///
    /// # Errors
    /// Returns [`CompileError`] as for [`compile_source`](Self::compile_source).
    /// Artifact validation failure is reported as an internal compiler error.
    pub fn compile_module(
        &self,
        source: &[u8],
        base: &CompileOptions,
    ) -> Result<crate::CompiledModule, CompileError> {
        let chunk = self.compile_source(source, base)?;
        self.compiled_module_from_chunk(chunk)
    }

    /// Compiles and validates `source` with the upstream-fixture option shape.
    #[doc(hidden)]
    pub fn compile_module_with_upstream_options(
        &self,
        source: &[u8],
        base: &UpstreamCompilerOptions,
    ) -> Result<crate::CompiledModule, CompileError> {
        let chunk = self.compile_source_with_upstream_options(source, base)?;
        self.compiled_module_from_chunk(chunk)
    }

    fn compiled_module_from_chunk(
        &self,
        chunk: BytecodeChunk,
    ) -> Result<crate::CompiledModule, CompileError> {
        crate::CompiledModule::new(chunk, self.clone()).map_err(|error| {
            CompileError::new(format!(
                "compiler produced a chunk that failed artifact validation: {error}"
            ))
        })
    }

    pub(crate) fn snapshot_bits(&self) -> (u16, bool) {
        let enabled = self
            .libraries
            .iter()
            .fold(0, |mask, library| mask | library.snapshot_bit());
        (enabled, self.runtime_compilation)
    }
}

impl Default for RuntimeCapabilities {
    /// Every library, with runtime compilation (`loadstring`) disabled.
    fn default() -> Self {
        Self::from_libraries(Library::ALL)
    }
}

fn canonical_libraries<I>(libraries: I) -> Vec<Library>
where
    I: IntoIterator<Item = Library>,
{
    let requested = libraries.into_iter().collect::<Vec<_>>();
    Library::ALL
        .into_iter()
        .filter(|library| requested.contains(library))
        .collect()
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn default_includes_every_library_and_empty_list_none() {
        let full = RuntimeCapabilities::default();
        let none = RuntimeCapabilities::from_libraries([]);
        for library in Library::ALL {
            assert!(full.includes(library), "default should include {library:?}");
            assert!(
                !none.includes(library),
                "empty list should exclude {library:?}"
            );
        }
    }

    #[test]
    fn explicit_library_lists_are_canonical_and_exact() {
        let only_math = RuntimeCapabilities::from_libraries([Library::Math]);
        assert!(only_math.includes(Library::Math));
        assert!(!only_math.includes(Library::Os));

        let no_debug = RuntimeCapabilities::from_libraries(
            Library::ALL
                .into_iter()
                .filter(|library| *library != Library::Debug),
        );
        assert!(!no_debug.includes(Library::Debug));
        assert!(no_debug.includes(Library::Math));

        let duplicate_ordered =
            RuntimeCapabilities::from_libraries([Library::Os, Library::Math, Library::Os]);
        assert_eq!(
            duplicate_ordered.libraries(),
            &[Library::Math, Library::Os],
            "library identity is canonical, not caller-order dependent"
        );
    }

    #[test]
    fn runtime_compilation_is_off_by_default_and_opt_in() {
        assert!(!RuntimeCapabilities::default().runtime_compilation_enabled());
        assert!(
            RuntimeCapabilities::default()
                .enable_runtime_compilation()
                .runtime_compilation_enabled()
        );
    }

    #[test]
    fn default_is_all_libraries_without_runtime_compilation() {
        assert_eq!(
            RuntimeCapabilities::default(),
            RuntimeCapabilities::from_libraries(Library::ALL),
            "the default runtime capabilities do not enable runtime compilation"
        );
    }

    /// Runs an erroring script under `capabilities`, returning the rendered
    /// error value and the captured traceback.
    fn erroring_run(capabilities: RuntimeCapabilities) -> (String, String) {
        let chunk = ruau_bytecode::compile_source(
            "local function inner()\n    error(\"boom\")\nend\nlocal function outer()\n    inner()\nend\nouter()\n",
            &ruau_bytecode::CompileOptions::default(),
            None,
        )
        .expect("compile");
        let mut vm = crate::Vm::builder()
            .runtime_capabilities(capabilities)
            .build_for_test();
        let module = vm.load_named(&chunk, b"@capabilities").expect("load");
        let error = vm
            .call_protected(&module, Default::default())
            .expect("an uncaught `error` is catchable, not fatal")
            .expect_err("the script raises");
        let text = match error.value() {
            crate::api::RawValue::String(handle) => {
                String::from_utf8_lossy(vm.heap().string(handle).expect("error string").bytes())
                    .into_owned()
            }
            other => panic!("expected a string error value, got {other:?}"),
        };
        let traceback = error
            .traceback()
            .expect("a protected entry captures a traceback")
            .to_owned();
        (text, traceback)
    }

    #[test]
    fn host_visible_error_locations_are_debug_capability_independent() {
        // The `debug` library gates script-visible introspection only; the
        // engine captures host-visible locations and tracebacks itself.
        let (with_debug_text, with_debug_traceback) = erroring_run(RuntimeCapabilities::default());
        let (no_debug_text, no_debug_traceback) =
            erroring_run(RuntimeCapabilities::from_libraries(
                Library::ALL
                    .into_iter()
                    .filter(|library| *library != Library::Debug),
            ));
        assert_eq!(with_debug_text, no_debug_text);
        assert_eq!(with_debug_traceback, no_debug_traceback);
        assert_eq!(with_debug_text, "capabilities:2: boom");
        assert!(
            with_debug_traceback.contains("capabilities:2")
                && with_debug_traceback.contains("capabilities:5"),
            "traceback should walk the inner and outer frames: {with_debug_traceback:?}"
        );
    }

    #[test]
    fn omitted_libraries_is_the_complement_and_names_are_ascii() {
        let capabilities = RuntimeCapabilities::from_libraries([Library::Math, Library::Table]);
        let omitted: Vec<Library> = capabilities.omitted_libraries().collect();
        assert!(omitted.contains(&Library::Os));
        assert!(!omitted.contains(&Library::Math));
        assert_eq!(omitted.len(), Library::ALL.len() - 2);
        assert_eq!(Library::Os.global_name(), "os");
    }
}
