//! In-process Luau type checking for Rust.
#![allow(clippy::missing_docs_in_private_items)]
//!
//! The crate wraps the Luau `Analysis` frontend through a C shim. A [`Checker`]
//! loads host definitions once, then type-checks any number of sources and
//! returns structured [`Diagnostic`]s. Checkers persist their definitions
//! across calls and are `Send` but not `Sync`.
//!
//! [`Checker::check`] takes a source string. [`Checker::check_path`] resolves
//! relative `require(...)` calls against the file's directory. Host-provided
//! in-memory modules flow through [`CheckOptions::virtual_modules`].
//!
//! # Example
//!
//! ```no_run
//! use ruau::analyzer::Checker;
//!
//! let mut checker = Checker::new().expect("native library load");
//! checker
//!     .add_definitions(
//!         r#"
//!         declare class TodoBuilder
//!             function content(self, content: string): TodoBuilder
//!         end
//!         declare Todo: { create: () -> TodoBuilder }
//!         "#,
//!     )
//!     .expect("definitions parse");
//!
//! let result = checker.check(
//!     r#"
//!     --!strict
//!     local _todo = Todo.create():content("review")
//!     "#,
//! );
//! assert!(result.is_ok());
//! ```

use std::{
    cell::RefCell, cmp::Ordering, collections::HashMap, error::Error as StdError, fmt, fs,
    marker::PhantomData, path::Path, ptr, rc::Rc, slice, sync::Arc, time::Duration,
};

pub use crate::module_schema::{
    ClassSchema, ModuleRoot, ModuleSchema, ModuleSchemaError, NamespaceSchema,
    extract_module_schema,
};
use crate::{
    Chunk, Luau, Table, Value, ffi,
    resolver::{ModuleId, ResolverSnapshot},
};

/// Default module label for source checks.
const DEFAULT_CHECK_MODULE_NAME: &str = "main";
/// Default module label for definition loading.
const DEFAULT_DEFINITIONS_MODULE_NAME: &str = "@definitions";

/// Diagnostic severity emitted by the checker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Type-check or lint error.
    Error,
    /// Lint warning.
    Warning,
}

/// A diagnostic produced by checking Luau source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Zero-based start line.
    pub line: u32,
    /// Zero-based start column.
    pub col: u32,
    /// Zero-based end line.
    pub end_line: u32,
    /// Zero-based end column.
    pub end_col: u32,
    /// Severity level.
    pub severity: Severity,
    /// Human-readable diagnostic message.
    pub message: String,
}

/// Result of a checker run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckResult {
    /// Collected diagnostics sorted by location and severity.
    pub diagnostics: Vec<Diagnostic>,
    /// Whether the check hit any time limit.
    pub timed_out: bool,
    /// Whether a cancellation request arrived during the check.
    pub cancelled: bool,
}

/// A parameter extracted from a direct functional entrypoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrypointParam {
    /// Parameter name in source order.
    pub name: String,
    /// Type annotation text as written.
    pub annotation: String,
    /// Whether the parameter is syntactically optional.
    pub optional: bool,
}

/// Parsed schema for a direct `return function(...) ... end` chunk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EntrypointSchema {
    /// Ordered parameter list for the returned function literal.
    pub params: Vec<EntrypointParam>,
}

impl CheckResult {
    /// Returns `true` when the result contains no errors.
    pub fn is_ok(&self) -> bool {
        !self.has_errors()
    }

    /// Returns `true` when the result contains any error.
    pub fn has_errors(&self) -> bool {
        self.has_severity(Severity::Error)
    }

    /// Returns `true` when the result contains any warning.
    pub fn has_warnings(&self) -> bool {
        self.has_severity(Severity::Warning)
    }

    /// Returns all error diagnostics.
    pub fn errors(&self) -> Vec<&Diagnostic> {
        self.diagnostics_with_severity(Severity::Error)
    }

    /// Returns all warning diagnostics.
    pub fn warnings(&self) -> Vec<&Diagnostic> {
        self.diagnostics_with_severity(Severity::Warning)
    }

    /// Returns all diagnostics matching the requested severity.
    fn diagnostics_with_severity(&self, severity: Severity) -> Vec<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == severity)
            .collect()
    }

    /// Returns whether any diagnostic matches the requested severity.
    fn has_severity(&self, severity: Severity) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == severity)
    }
}

impl Severity {
    /// Returns the severity as a stable lowercase string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }

    /// Converts the shim severity code into the public enum.
    fn from_ffi(code: u32) -> Self {
        match code {
            0 => Self::Error,
            _ => Self::Warning,
        }
    }
}

/// Compile-time policy values this crate's checker operates under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckerPolicy {
    /// Whether strict mode is always enforced.
    pub strict_mode: bool,
    /// Active solver policy string.
    pub solver: &'static str,
    /// Whether the crate exposes batch queue support.
    pub exposes_batch_queue: bool,
}

/// Returns the fixed checker policy.
pub const fn checker_policy() -> CheckerPolicy {
    CheckerPolicy {
        strict_mode: true,
        solver: "new",
        exposes_batch_queue: false,
    }
}

/// Errors returned by checker construction, source checking, and definition loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The native layer failed to create the checker.
    CreateCheckerFailed,
    /// The native layer failed to create the cancellation token.
    CreateCancellationTokenFailed,
    /// Definitions failed to parse or type-check.
    Definitions(String),
    /// A checked load was rejected by analyzer diagnostics.
    CheckFailed(CheckResult),
    /// A checked-load snapshot did not contain its root module.
    MissingSnapshotRoot(String),
    /// Checked-load runtime setup failed after analysis passed.
    Load(String),
    /// Entrypoint schema extraction failed.
    EntrypointSchema(String),
    /// Failed to read a UTF-8 text file for checking or definition loading.
    ReadFile {
        /// Logical input category such as `"source"` or `"definitions"`.
        kind: &'static str,
        /// Display label for the file path.
        path: String,
        /// Human-readable I/O error message.
        message: String,
    },
    /// UTF-8 input is too large for the C ABI length type.
    InputTooLarge {
        /// Logical input category such as `"source"` or `"definitions"`.
        kind: &'static str,
        /// Original input length in bytes.
        len: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateCheckerFailed => formatter.write_str("failed to create Luau checker"),
            Self::CreateCancellationTokenFailed => {
                formatter.write_str("failed to create Luau cancellation token")
            }
            Self::Definitions(message) => {
                write!(formatter, "failed to load Luau definitions: {message}")
            }
            Self::CheckFailed(result) => write!(
                formatter,
                "checked load failed with {} diagnostic(s)",
                result.diagnostics.len()
            ),
            Self::MissingSnapshotRoot(root) => {
                write!(
                    formatter,
                    "resolver snapshot is missing root module `{root}`"
                )
            }
            Self::Load(message) => write!(formatter, "failed to prepare checked load: {message}"),
            Self::EntrypointSchema(message) => {
                write!(
                    formatter,
                    "failed to extract Luau entrypoint schema: {message}"
                )
            }
            Self::ReadFile {
                kind,
                path,
                message,
            } => {
                write!(formatter, "failed to read {kind} `{path}`: {message}")
            }
            Self::InputTooLarge { kind, len } => {
                write!(
                    formatter,
                    "{kind} input is too large for checker FFI boundary ({len} bytes)"
                )
            }
        }
    }
}

impl StdError for Error {}

/// Default checker configuration used by `Checker`.
#[derive(Debug, Clone)]
pub struct CheckerOptions {
    /// Optional timeout applied to checks that do not override it.
    pub default_timeout: Option<Duration>,
    /// Default module label used for source checks.
    pub default_module_name: String,
    /// Default module label used for definition loading.
    pub default_definitions_module_name: String,
}

impl Default for CheckerOptions {
    fn default() -> Self {
        Self {
            default_timeout: None,
            default_module_name: DEFAULT_CHECK_MODULE_NAME.to_owned(),
            default_definitions_module_name: DEFAULT_DEFINITIONS_MODULE_NAME.to_owned(),
        }
    }
}

/// A host-provided virtual module visible to a single check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualModule<'a> {
    /// Module name used by `require(...)`, for example `"term"`.
    pub name: &'a str,
    /// Source code returned by the host for that module.
    pub source: &'a str,
}

/// Per-call options for `Checker::check_with_options`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CheckOptions<'a> {
    /// Optional timeout override for this call.
    pub timeout: Option<Duration>,
    /// Optional module label override for this call.
    ///
    /// For source that uses relative `require(...)`, this must identify a real
    /// filesystem module path so the checker can resolve adjacent files.
    pub module_name: Option<&'a str>,
    /// Optional cancellation token for this call.
    pub cancellation_token: Option<&'a CancellationToken>,
    /// Optional host-provided virtual modules visible to this call.
    pub virtual_modules: &'a [VirtualModule<'a>],
}

impl<'a> CheckOptions<'a> {
    /// Supplies a module name only when the caller did not provide one already.
    fn with_fallback_module_name(self, module_name: &'a str) -> Self {
        Self {
            module_name: self.module_name.or(Some(module_name)),
            ..self
        }
    }
}

/// A reusable cancellation token. Signal it from any thread to interrupt a
/// running check.
///
/// `CancellationToken` is `Send` and `Sync`: the underlying Luau implementation
/// manages signaled state through atomic operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    /// Shared token internals.
    inner: Arc<CancellationTokenInner>,
}

/// Shared cancellation token internals.
#[derive(Debug)]
struct CancellationTokenInner {
    /// Raw C cancellation token handle.
    raw: ffi::RuauTokenHandle,
}

// The underlying C cancellation token uses atomic state and is thread-safe for signal/reset.
unsafe impl Send for CancellationTokenInner {}
// The underlying C cancellation token uses atomic state and is thread-safe for signal/reset.
unsafe impl Sync for CancellationTokenInner {}

impl Drop for CancellationTokenInner {
    fn drop(&mut self) {
        // SAFETY: `raw` originates from `ruau_cancellation_token_new` and is valid until drop.
        unsafe { ffi::ruau_cancellation_token_free(self.raw) };
    }
}

impl CancellationToken {
    /// Creates a new cancellation token.
    pub fn new() -> Result<Self, Error> {
        // SAFETY: Calling into shim constructor. Null indicates failure.
        let raw = unsafe { ffi::ruau_cancellation_token_new() };
        if raw.is_null() {
            return Err(Error::CreateCancellationTokenFailed);
        }
        Ok(Self {
            inner: Arc::new(CancellationTokenInner { raw }),
        })
    }

    /// Requests cancellation on this token.
    pub fn cancel(&self) {
        // SAFETY: `raw` is valid while `inner` is alive.
        unsafe { ffi::ruau_cancellation_token_cancel(self.inner.raw) };
    }

    /// Clears cancellation state on this token.
    pub fn reset(&self) {
        // SAFETY: `raw` is valid while `inner` is alive.
        unsafe { ffi::ruau_cancellation_token_reset(self.inner.raw) };
    }

    /// Returns the raw C token handle.
    fn raw(&self) -> ffi::RuauTokenHandle {
        self.inner.raw
    }
}

/// Reusable checker instance with persistent global definitions.
///
/// `Checker` is `Send` but not `Sync`. The underlying Luau Analysis structures
/// are safely movable between threads, but all operations that mutate or read
/// from the checker require exclusive `&mut self` access, meaning it cannot
/// be concurrently accessed from multiple threads.
pub struct Checker {
    /// Opaque handle to the native checker instance.
    inner: ffi::RuauCheckerHandle,
    /// Default checker behavior options.
    options: CheckerOptions,
}

// The underlying checker is single-threaded (`&mut self` methods), but ownership can move.
unsafe impl Send for Checker {}

impl Checker {
    /// Creates a checker with default options.
    pub fn new() -> Result<Self, Error> {
        Self::with_options(CheckerOptions::default())
    }

    /// Creates a checker with explicit defaults.
    pub fn with_options(options: CheckerOptions) -> Result<Self, Error> {
        // SAFETY: Calling into shim constructor. Null indicates failure.
        let inner = unsafe { ffi::ruau_checker_new() };
        if inner.is_null() {
            return Err(Error::CreateCheckerFailed);
        }
        Ok(Self { inner, options })
    }

    /// Returns the checker's default options.
    pub fn options(&self) -> &CheckerOptions {
        &self.options
    }

    /// Loads Luau definition source using the default module label.
    pub fn add_definitions(&mut self, defs: &str) -> Result<(), Error> {
        add_definitions_raw(
            self.inner,
            defs,
            &self.options.default_definitions_module_name,
        )
    }

    /// Loads Luau definitions from a UTF-8 text file using the path as module label.
    pub fn add_definitions_path(&mut self, path: &Path) -> Result<(), Error> {
        let defs = LoadedInput::read(path, "definitions")?;
        prefix_definitions_error(
            &defs.label,
            add_definitions_raw(self.inner, &defs.contents, &defs.label),
        )
    }

    /// Loads Luau definition source with an explicit module label.
    pub fn add_definitions_with_name(
        &mut self,
        defs: &str,
        module_name: &str,
    ) -> Result<(), Error> {
        add_definitions_raw(self.inner, defs, module_name)
    }

    /// Type-checks a Luau source module with default options.
    pub fn check(&mut self, source: &str) -> Result<CheckResult, Error> {
        self.check_with_options(source, CheckOptions::default())
    }

    /// Type-checks a Luau source file with default options and the path as module label.
    pub fn check_path(&mut self, path: &Path) -> Result<CheckResult, Error> {
        self.check_path_with_options(path, CheckOptions::default())
    }

    /// Type-checks a Luau source file with explicit per-call options.
    ///
    /// Relative `require(...)` calls resolve against the file path unless
    /// `options.module_name` supplies a different module label.
    pub fn check_path_with_options(
        &mut self,
        path: &Path,
        options: CheckOptions<'_>,
    ) -> Result<CheckResult, Error> {
        let source = LoadedInput::read(path, "source")?;
        self.check_with_options(
            &source.contents,
            options.with_fallback_module_name(source.label.as_str()),
        )
    }

    /// Type-checks a pre-resolved module graph.
    pub fn check_snapshot(&mut self, snapshot: &ResolverSnapshot) -> Result<CheckResult, Error> {
        let root = snapshot
            .root_source()
            .ok_or_else(|| Error::MissingSnapshotRoot(snapshot.root.as_str().to_owned()))?;
        let virtual_modules = snapshot.virtual_modules();
        self.check_with_options(
            root.source.as_str(),
            CheckOptions {
                module_name: Some(root.id.as_str()),
                virtual_modules: &virtual_modules,
                ..CheckOptions::default()
            },
        )
    }

    /// Type-checks a Luau source module with explicit per-call options.
    pub fn check_with_options(
        &mut self,
        source: &str,
        options: CheckOptions<'_>,
    ) -> Result<CheckResult, Error> {
        let source = FfiStr::new(source, "source")?;
        let options = ResolvedCheckOptions::new(options, &self.options)?;
        let raw_options = options.as_ffi();

        // SAFETY: Input pointers and checker handle are valid for call duration.
        let raw = unsafe {
            ffi::ruau_checker_check(self.inner, source.ptr(), source.len(), &raw_options)
        };
        let raw = RawGuard::new(raw);
        let raw = raw.as_ref();

        let mut diagnostics = collect_diagnostics(raw);
        diagnostics.sort_by(diagnostic_sort_key);
        Ok(CheckResult {
            diagnostics,
            timed_out: raw.timed_out != 0,
            cancelled: raw.cancelled != 0,
        })
    }
}

impl Luau {
    /// Type-checks a resolver snapshot before returning a loadable root chunk.
    pub fn checked_load<'a>(
        &self,
        checker: &mut Checker,
        snapshot: &'a ResolverSnapshot,
    ) -> Result<Chunk<'a>, Error> {
        let result = checker.check_snapshot(snapshot)?;
        if !result.is_ok() {
            return Err(Error::CheckFailed(result));
        }

        let root = snapshot
            .root_source()
            .ok_or_else(|| Error::MissingSnapshotRoot(snapshot.root.as_str().to_owned()))?;
        let snapshot = Arc::new(snapshot.clone());
        let cache = Rc::new(RefCell::new(HashMap::new()));
        let env = snapshot_environment(
            self,
            Arc::clone(&snapshot),
            Rc::clone(&cache),
            root.id.clone(),
        )
        .map_err(|error| Error::Load(error.to_string()))?;

        Ok(self
            .load(root.source.as_str())
            .set_name(root.id.as_str())
            .set_environment(env))
    }
}

/// Checked-load module-result cache.
type ModuleCache = Rc<RefCell<HashMap<ModuleId, Value>>>;

/// Builds an environment whose `require` function reads from a resolver snapshot.
fn snapshot_environment(
    lua: &Luau,
    snapshot: Arc<ResolverSnapshot>,
    cache: ModuleCache,
    requester: ModuleId,
) -> crate::Result<Table> {
    let env = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.raw_set("__index", lua.globals())?;
    env.set_metatable(Some(metatable))?;

    let require = lua.create_function(move |lua, specifier: String| {
        snapshot_require(lua, &snapshot, &cache, &requester, &specifier)
    })?;
    env.raw_set("require", require)?;
    Ok(env)
}

/// Loads one dependency from a checked resolver snapshot.
fn snapshot_require(
    lua: &Luau,
    snapshot: &Arc<ResolverSnapshot>,
    cache: &ModuleCache,
    requester: &ModuleId,
    specifier: &str,
) -> crate::Result<Value> {
    let module = snapshot
        .dependency(requester, specifier)
        .ok_or_else(|| crate::Error::runtime(format!("module not found: {specifier}")))?;
    if let Some(value) = cache.borrow().get(&module.id).cloned() {
        return Ok(value);
    }

    let env = snapshot_environment(
        lua,
        Arc::clone(snapshot),
        Rc::clone(cache),
        module.id.clone(),
    )?;
    let value = lua
        .load(module.source.as_str())
        .set_name(module.id.as_str())
        .set_environment(env)
        .call_sync::<Value>(())?;
    cache.borrow_mut().insert(module.id.clone(), value.clone());
    Ok(value)
}

/// Extracts parameter names, annotation text, and optionality from a direct
/// `return function(...) ... end` chunk.
pub fn extract_entrypoint_schema(source: &str) -> Result<EntrypointSchema, Error> {
    let source = FfiStr::new(source, "source")?;
    // SAFETY: Input pointer is valid for the call duration.
    let raw = unsafe { ffi::ruau_extract_entrypoint_schema(source.ptr(), source.len()) };
    let raw = RawGuard::new(raw);

    if raw.as_ref().error_len != 0 {
        return Err(Error::EntrypointSchema(string_from_raw(
            raw.as_ref().error,
            raw.as_ref().error_len,
        )));
    }

    Ok(EntrypointSchema {
        params: collect_entrypoint_params(raw.as_ref()),
    })
}

impl Drop for Checker {
    fn drop(&mut self) {
        // SAFETY: `self.inner` originates from `ruau_checker_new` and is valid until drop.
        unsafe { ffi::ruau_checker_free(self.inner) };
    }
}

/// Loads Luau definition source through the native checker with a chosen module label.
fn add_definitions_raw(
    checker: ffi::RuauCheckerHandle,
    defs: &str,
    module_name: &str,
) -> Result<(), Error> {
    let defs = FfiStr::new(defs, "definitions")?;
    let module_name = FfiStr::new(module_name, "definition module name")?;

    // SAFETY: Pointers are valid for call duration and checker handle is live.
    let raw = RawGuard::new(unsafe {
        ffi::ruau_checker_add_definitions(
            checker,
            defs.ptr(),
            defs.len(),
            module_name.ptr(),
            module_name.len(),
        )
    });

    let string = raw.as_ref();
    if string.len == 0 {
        Ok(())
    } else {
        Err(Error::Definitions(string_from_raw(string.data, string.len)))
    }
}

/// UTF-8 checker input loaded from disk together with its display label.
struct LoadedInput {
    /// Display label used for diagnostics and module names.
    label: String,
    /// UTF-8 contents loaded from disk.
    contents: String,
}

impl LoadedInput {
    /// Reads one UTF-8 file used as checker input.
    fn read(path: &Path, kind: &'static str) -> Result<Self, Error> {
        let label = path.display().to_string();
        let contents = fs::read_to_string(path).map_err(|error| Error::ReadFile {
            kind,
            path: label.clone(),
            message: error.to_string(),
        })?;
        Ok(Self { label, contents })
    }
}

/// Borrowed UTF-8 input prepared for a C ABI call.
#[derive(Clone, Copy)]
struct FfiStr<'a> {
    /// Pointer to the UTF-8 bytes, or null for empty strings.
    ptr: *const u8,
    /// Length of the UTF-8 payload in bytes.
    len: u32,
    /// Ties the raw pointer to the borrowed Rust string lifetime.
    _marker: PhantomData<&'a str>,
}

impl<'a> FfiStr<'a> {
    /// Converts a Rust string to a pointer-length pair accepted by the C ABI.
    fn new(value: &'a str, kind: &'static str) -> Result<Self, Error> {
        let len = u32::try_from(value.len()).map_err(|_| Error::InputTooLarge {
            kind,
            len: value.len(),
        })?;

        Ok(Self {
            ptr: if len == 0 {
                ptr::null()
            } else {
                value.as_ptr()
            },
            len,
            _marker: PhantomData,
        })
    }

    /// Returns the UTF-8 pointer for the C ABI.
    fn ptr(self) -> *const u8 {
        self.ptr
    }

    /// Returns the UTF-8 byte length for the C ABI.
    fn len(self) -> u32 {
        self.len
    }
}

/// Check options after merging checker defaults and converting FFI handles.
struct ResolvedCheckOptions<'a> {
    /// Module label prepared for the C ABI.
    module_name: FfiStr<'a>,
    /// Timeout value after falling back to checker defaults.
    timeout: Option<Duration>,
    /// Raw cancellation token handle, or null when disabled.
    cancellation_token: ffi::RuauTokenHandle,
    /// Virtual modules prepared for the C ABI.
    virtual_modules: ResolvedVirtualModules<'a>,
}

impl<'a> ResolvedCheckOptions<'a> {
    /// Merges per-call options with checker defaults.
    fn new(options: CheckOptions<'a>, defaults: &'a CheckerOptions) -> Result<Self, Error> {
        Ok(Self {
            module_name: FfiStr::new(
                options
                    .module_name
                    .unwrap_or(defaults.default_module_name.as_str()),
                "module name",
            )?,
            timeout: options.timeout.or(defaults.default_timeout),
            cancellation_token: options
                .cancellation_token
                .map_or(ffi::RuauTokenHandle::null(), CancellationToken::raw),
            virtual_modules: ResolvedVirtualModules::new(options.virtual_modules)?,
        })
    }

    /// Converts resolved options into the raw ABI form expected by the shim.
    fn as_ffi(&self) -> ffi::RuauCheckOptions {
        ffi::RuauCheckOptions {
            module_name: self.module_name.ptr(),
            module_name_len: self.module_name.len(),
            has_timeout: u32::from(self.timeout.is_some()),
            timeout_seconds: self.timeout.map_or(0.0, |duration| duration.as_secs_f64()),
            cancellation_token: self.cancellation_token,
            virtual_modules: self.virtual_modules.ptr(),
            virtual_module_count: self.virtual_modules.len(),
        }
    }
}

/// Virtual modules after converting borrowed strings to C ABI pointers.
///
/// The raw pointers inside `entries` borrow from the original caller-owned
/// strings, tracked by the `'a` lifetime parameter.
struct ResolvedVirtualModules<'a> {
    /// Raw ABI entries borrowing from the caller-owned module strings.
    entries: Vec<ffi::RuauVirtualModule>,
    /// Ties the borrowed raw pointers to the input lifetime.
    _marker: PhantomData<&'a ()>,
}

impl<'a> ResolvedVirtualModules<'a> {
    /// Converts borrowed virtual modules into ABI-safe storage.
    fn new(modules: &'a [VirtualModule<'a>]) -> Result<Self, Error> {
        let entries = modules
            .iter()
            .map(|module| {
                let name = FfiStr::new(module.name, "virtual module name")?;
                let source = FfiStr::new(module.source, "virtual module source")?;
                Ok(ffi::RuauVirtualModule {
                    name: name.ptr(),
                    name_len: name.len(),
                    source: source.ptr(),
                    source_len: source.len(),
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(Self {
            entries,
            _marker: PhantomData,
        })
    }

    /// Returns the ABI pointer to the first virtual module entry.
    fn ptr(&self) -> *const ffi::RuauVirtualModule {
        if self.entries.is_empty() {
            ptr::null()
        } else {
            self.entries.as_ptr()
        }
    }

    /// Returns the number of ABI entries.
    fn len(&self) -> u32 {
        u32::try_from(self.entries.len()).expect("virtual module count should fit in u32")
    }
}

/// A shim-allocated FFI resource that is released through a fixed entrypoint.
trait FfiResource: Copy {
    /// Releases the resource through its native free function.
    ///
    /// # Safety
    ///
    /// The value must originate from the matching shim allocator and must not have
    /// been released already.
    unsafe fn release(self);
}

impl FfiResource for ffi::RuauCheckResult {
    unsafe fn release(self) {
        // SAFETY: Caller guarantees this value came from `ruau_checker_check`.
        unsafe { ffi::ruau_check_result_free(self) };
    }
}

impl FfiResource for ffi::RuauString {
    unsafe fn release(self) {
        // SAFETY: Caller guarantees this value came from a shim entrypoint that returns `LuauString`.
        unsafe { ffi::ruau_string_free(self) };
    }
}

impl FfiResource for ffi::RuauEntrypointSchemaResult {
    unsafe fn release(self) {
        // SAFETY: Caller guarantees this value came from `ruau_extract_entrypoint_schema`.
        unsafe { ffi::ruau_entrypoint_schema_result_free(self) };
    }
}

/// RAII guard that releases a shim-allocated FFI resource on scope exit.
struct RawGuard<T: FfiResource> {
    /// Raw resource allocated by the shim.
    raw: T,
}

impl<T: FfiResource> RawGuard<T> {
    /// Creates a guard for a shim-allocated resource.
    fn new(raw: T) -> Self {
        Self { raw }
    }

    /// Returns a shared reference to the underlying resource.
    fn as_ref(&self) -> &T {
        &self.raw
    }
}

impl<T: FfiResource> Drop for RawGuard<T> {
    fn drop(&mut self) {
        // SAFETY: `raw` originated from the shim and must be released exactly once.
        unsafe { self.raw.release() };
    }
}

/// Adds the file label to definitions failures produced by the native layer.
fn prefix_definitions_error(label: &str, result: Result<(), Error>) -> Result<(), Error> {
    match result {
        Err(Error::Definitions(message)) => Err(Error::Definitions(format!("{label}: {message}"))),
        other => other,
    }
}

/// Converts raw UTF-8 bytes from C into a Rust `String`.
fn string_from_raw(ptr: *const u8, len: u32) -> String {
    if ptr.is_null() || len == 0 {
        return String::new();
    }

    // SAFETY: `ptr` points to `len` bytes provided by the shim for this call scope.
    let bytes = unsafe { slice::from_raw_parts(ptr, len as usize) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Converts diagnostic rows owned by the shim into Rust values.
fn collect_diagnostics(raw: &ffi::RuauCheckResult) -> Vec<Diagnostic> {
    // SAFETY: `raw.diagnostics` points to `diagnostic_count` entries owned by `raw`.
    unsafe { raw_slice(raw.diagnostics, raw.diagnostic_count) }
        .iter()
        .map(|diagnostic| Diagnostic {
            line: diagnostic.line,
            col: diagnostic.col,
            end_line: diagnostic.end_line,
            end_col: diagnostic.end_col,
            severity: Severity::from_ffi(diagnostic.severity),
            message: string_from_raw(diagnostic.message, diagnostic.message_len),
        })
        .collect()
}

/// Converts entrypoint parameter rows owned by the shim into Rust values.
fn collect_entrypoint_params(raw: &ffi::RuauEntrypointSchemaResult) -> Vec<EntrypointParam> {
    // SAFETY: `raw.params` points to `param_count` entries owned by `raw`.
    unsafe { raw_slice(raw.params, raw.param_count) }
        .iter()
        .map(|param| EntrypointParam {
            name: string_from_raw(param.name, param.name_len),
            annotation: string_from_raw(param.annotation, param.annotation_len),
            optional: param.optional != 0,
        })
        .collect()
}

/// Forms a borrowed slice from a non-owning C pointer and element count.
unsafe fn raw_slice<'a, T>(ptr: *const T, len: u32) -> &'a [T] {
    if len == 0 {
        &[]
    } else {
        debug_assert!(!ptr.is_null(), "non-empty shim slice must not be null");
        // SAFETY: The caller guarantees `ptr` is valid for `len` elements.
        unsafe { slice::from_raw_parts(ptr, len as usize) }
    }
}

/// Sorts diagnostics by location, then severity, then message.
fn diagnostic_sort_key(left: &Diagnostic, right: &Diagnostic) -> Ordering {
    left.line
        .cmp(&right.line)
        .then(left.col.cmp(&right.col))
        .then(left.severity.cmp(&right.severity))
        .then(left.message.cmp(&right.message))
}

/// Unit tests for public result helpers and policy defaults.
#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        CheckResult, Checker, CheckerOptions, Diagnostic, Error, Severity, checker_policy,
        extract_entrypoint_schema,
    };

    /// Verifies `CheckResult::is_ok` is true for warning-only results.
    #[test]
    fn check_result_ok_with_warnings() {
        let result = CheckResult {
            diagnostics: vec![Diagnostic {
                line: 0,
                col: 0,
                end_line: 0,
                end_col: 1,
                severity: Severity::Warning,
                message: "unused local".to_owned(),
            }],
            timed_out: false,
            cancelled: false,
        };

        assert!(result.is_ok());
        assert_eq!(1, result.warnings().len());
        assert_eq!(0, result.errors().len());
    }

    /// Verifies `CheckResult::is_ok` is false when at least one error exists.
    #[test]
    fn check_result_not_ok_with_error() {
        let result = CheckResult {
            diagnostics: vec![Diagnostic {
                line: 1,
                col: 1,
                end_line: 1,
                end_col: 5,
                severity: Severity::Error,
                message: "type mismatch".to_owned(),
            }],
            timed_out: false,
            cancelled: false,
        };

        assert!(!result.is_ok());
        assert_eq!(0, result.warnings().len());
        assert_eq!(1, result.errors().len());
    }

    /// Verifies policy constants match project decisions.
    #[test]
    fn policy_is_strict_new_solver_and_queue_free() {
        let policy = checker_policy();
        assert!(policy.strict_mode);
        assert_eq!("new", policy.solver);
        assert!(!policy.exposes_batch_queue);
    }

    /// Verifies checker options defaults use stable module labels.
    #[test]
    fn checker_options_defaults_are_stable() {
        let options = CheckerOptions::default();
        assert_eq!("main", options.default_module_name);
        assert_eq!("@definitions", options.default_definitions_module_name);
        assert!(options.default_timeout.is_none());
    }

    /// Verifies schema extraction reads direct function parameters in order.
    #[test]
    fn extract_entrypoint_schema_reads_params() {
        let schema = extract_entrypoint_schema(
            r#"
return function(target: Node, count: number?, payload: JsonValue)
    return nil
end
"#,
        )
        .expect("schema");
        assert_eq!(3, schema.params.len());
        assert_eq!("target", schema.params[0].name);
        assert_eq!("Node", schema.params[0].annotation);
        assert!(!schema.params[0].optional);
        assert_eq!("count", schema.params[1].name);
        assert_eq!("number?", schema.params[1].annotation);
        assert!(schema.params[1].optional);
        assert_eq!("payload", schema.params[2].name);
        assert_eq!("JsonValue", schema.params[2].annotation);
        assert!(!schema.params[2].optional);
    }

    /// Verifies schema extraction rejects indirect entrypoints.
    #[test]
    fn extract_entrypoint_schema_rejects_indirect_return() {
        let error = extract_entrypoint_schema(
            r#"
local main = function(target: Node)
    return nil
end
return main
"#,
        )
        .expect_err("schema should fail");
        assert!(
            error
                .to_string()
                .contains("script must use a direct `return function(...) ... end` entrypoint"),
            "{error}"
        );
    }

    /// Verifies path-based source checks surface readable file errors.
    #[test]
    fn check_path_reports_read_error() {
        let mut checker = Checker::new().expect("checker creation should succeed");
        let missing = temp_path("missing_source");

        let error = checker
            .check_path(&missing)
            .expect_err("missing file should fail");
        match error {
            Error::ReadFile {
                kind,
                path,
                message,
            } => {
                assert_eq!("source", kind);
                assert_eq!(missing.display().to_string(), path);
                assert!(
                    !message.is_empty(),
                    "read error message should not be empty"
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Verifies path-based definitions loading reads UTF-8 files and preserves labels.
    #[test]
    fn add_definitions_path_loads_file_contents() {
        let mut checker = Checker::new().expect("checker creation should succeed");
        let path = temp_path("definitions");
        fs::write(&path, "declare function file_defined(): string\n")
            .expect("definitions file should be written");

        checker
            .add_definitions_path(&path)
            .expect("definitions path should load");
        let result = checker
            .check(
                r#"
            --!strict
            local value: string = file_defined()
            "#,
            )
            .expect("source should check");

        fs::remove_file(&path).expect("temp file should be removed");
        assert!(result.is_ok(), "path-loaded definitions should stay active");
    }

    /// Creates a unique temp file path for filesystem tests.
    fn temp_path(stem: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        env::temp_dir().join(format!("ruau-{stem}-{unique}.luau"))
    }
}
