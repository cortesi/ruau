//! VM standard-library and base-surface selection.
//!
//! A [`Profile`] selects optional library tables (`math`, `os`, `buffer`, ...)
//! and capabilities such as runtime compilation.

/// An optional standard library a [`Profile`] can include or omit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Library {
    /// `coroutine` — create/resume/yield/status/wrap/…
    Coroutine,
    /// `string` — patterns, `format`, `pack`, plus the string metatable.
    String,
    /// `math` — the numeric surface and its constants.
    Math,
    /// `integer` — 64-bit integer construction and bit/arithmetic helpers.
    Integer,
    /// `table` — insert/remove/sort/pack/move/freeze/…
    Table,
    /// `bit32` — 32-bit bitwise operations.
    Bit32,
    /// `utf8` — codepoint iteration and `charpattern`.
    Utf8,
    /// `os` — the time-only surface (no filesystem or process access).
    Os,
    /// `buffer` — fixed-size little-endian byte buffers.
    Buffer,
    /// `vector` — 3-component float vectors and their constants.
    Vector,
    /// `debug` — script-visible `debug.info` and `debug.traceback`.
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

    /// The global name the library installs under (`math`, `os`, …), as the
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

    /// The global name as a string (the [`global_name_bytes`](Self::global_name_bytes)
    /// bytes, which are ASCII) — what the compiler/type-checker surfaces key
    /// globals by.
    #[must_use]
    pub fn global_name(self) -> &'static str {
        std::str::from_utf8(self.global_name_bytes()).expect("library names are ASCII")
    }

    /// This library's bit in a [`Profile`] mask.
    fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

/// Selects which standard libraries and base-surface capabilities a VM installs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Profile {
    /// One bit per [`Library`] (`Library as u16`); a set bit means installed.
    enabled: u16,
    /// Whether `loadstring` is installed as a base global.
    runtime_compilation: bool,
}

impl Profile {
    /// Every standard library (the default).
    #[must_use]
    pub fn full() -> Self {
        let enabled = Library::ALL
            .into_iter()
            .fold(0, |mask, lib| mask | lib.bit());
        Self {
            enabled,
            runtime_compilation: true,
        }
    }

    /// Only the base globals — no optional library tables. Runtime compilation
    /// remains enabled unless [`without_runtime_compilation`](Self::without_runtime_compilation)
    /// is applied.
    #[must_use]
    pub fn base_only() -> Self {
        Self {
            enabled: 0,
            runtime_compilation: true,
        }
    }

    /// Whether `library` is installed under this profile.
    #[must_use]
    pub fn includes(self, library: Library) -> bool {
        self.enabled & library.bit() != 0
    }

    /// This profile with `library` added.
    #[must_use]
    pub fn with(mut self, library: Library) -> Self {
        self.enabled |= library.bit();
        self
    }

    /// This profile with `library` removed.
    #[must_use]
    pub fn without(mut self, library: Library) -> Self {
        self.enabled &= !library.bit();
        self
    }

    /// Whether runtime compilation through `loadstring` is installed.
    #[must_use]
    pub fn runtime_compilation_enabled(self) -> bool {
        self.runtime_compilation
    }

    /// Enables runtime compilation through `loadstring`.
    #[must_use]
    pub fn with_runtime_compilation(mut self) -> Self {
        self.runtime_compilation = true;
        self
    }

    /// Disables runtime compilation by omitting `loadstring` from the base
    /// globals. Precompiled bytecode can still be loaded through the host API.
    #[must_use]
    pub fn without_runtime_compilation(mut self) -> Self {
        self.runtime_compilation = false;
        self
    }

    /// The libraries this profile omits — the complement of what it installs.
    /// The compiler and type-checker surfaces use these to drop a disabled
    /// library's global from optimization and from name resolution.
    pub fn omitted_libraries(self) -> impl Iterator<Item = Library> {
        Library::ALL
            .into_iter()
            .filter(move |&library| !self.includes(library))
    }

    pub(crate) fn snapshot_bits(self) -> (u16, bool) {
        (self.enabled, self.runtime_compilation)
    }
}

impl Default for Profile {
    /// Every library, with runtime compilation (`loadstring`) disabled: the
    /// default posture should not hand untrusted code a compiler. Opt in
    /// explicitly with [`Profile::full`] (which keeps it enabled) when the
    /// embedding wants it.
    fn default() -> Self {
        Self::full().without_runtime_compilation()
    }
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn full_includes_every_library_and_base_only_none() {
        let full = Profile::full();
        let none = Profile::base_only();
        for library in Library::ALL {
            assert!(full.includes(library), "full should include {library:?}");
            assert!(
                !none.includes(library),
                "base_only should exclude {library:?}"
            );
        }
    }

    #[test]
    fn with_and_without_toggle_one_library() {
        let only_math = Profile::base_only().with(Library::Math);
        assert!(only_math.includes(Library::Math));
        assert!(!only_math.includes(Library::Os));

        let no_debug = Profile::full().without(Library::Debug);
        assert!(!no_debug.includes(Library::Debug));
        assert!(no_debug.includes(Library::Math));
    }

    #[test]
    fn runtime_compilation_is_explicitly_toggled() {
        assert!(Profile::full().runtime_compilation_enabled());
        assert!(Profile::base_only().runtime_compilation_enabled());

        let disabled = Profile::full().without_runtime_compilation();
        assert!(!disabled.runtime_compilation_enabled());
        assert!(
            disabled
                .with_runtime_compilation()
                .runtime_compilation_enabled()
        );
    }

    #[test]
    fn default_is_full() {
        assert_eq!(
            Profile::default(),
            Profile::full().without_runtime_compilation(),
            "the default profile does not enable runtime compilation"
        );
    }

    /// Runs an erroring script under `profile`, returning the rendered error
    /// value and the captured traceback.
    fn erroring_run(profile: Profile) -> (String, String) {
        let chunk = ruau_bytecode::compile_source(
            "local function inner()\n    error(\"boom\")\nend\nlocal function outer()\n    inner()\nend\nouter()\n",
            &ruau_bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
        let mut vm = crate::Vm::builder().profile(profile).build_for_test();
        let module = vm.load_named(&chunk, b"@profiled").expect("load");
        let error = vm
            .call_protected(&module, Default::default())
            .expect("an uncaught `error` is catchable, not fatal")
            .expect_err("the script raises");
        let text = match error.value() {
            ruau_vm_api::RawValue::String(handle) => {
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
    fn host_visible_error_locations_are_debug_profile_independent() {
        // The `debug` library gates script-visible introspection only; the
        // engine captures host-visible locations and tracebacks itself. The
        // same erroring script must therefore report byte-identical error
        // text and traceback with `Library::Debug` profiled out.
        let (with_debug_text, with_debug_traceback) = erroring_run(Profile::full());
        let (no_debug_text, no_debug_traceback) =
            erroring_run(Profile::full().without(Library::Debug));
        assert_eq!(with_debug_text, no_debug_text);
        assert_eq!(with_debug_traceback, no_debug_traceback);
        // And the shared output is a real location + traceback, not two
        // matching empties: the message carries the raise site, the traceback
        // the frame chain.
        assert_eq!(with_debug_text, "profiled:2: boom");
        assert!(
            with_debug_traceback.contains("profiled:2")
                && with_debug_traceback.contains("profiled:5"),
            "traceback should walk the inner and outer frames: {with_debug_traceback:?}"
        );
    }

    #[test]
    fn omitted_libraries_is_the_complement_and_names_are_ascii() {
        let profile = Profile::base_only()
            .with(Library::Math)
            .with(Library::Table);
        let omitted: Vec<Library> = profile.omitted_libraries().collect();
        assert!(omitted.contains(&Library::Os));
        assert!(!omitted.contains(&Library::Math));
        assert_eq!(omitted.len(), Library::ALL.len() - 2);
        assert_eq!(Library::Os.global_name(), "os");
    }
}
