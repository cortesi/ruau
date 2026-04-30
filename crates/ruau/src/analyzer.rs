//! In-process Luau type checking for Rust.
#![allow(clippy::missing_docs_in_private_items)]
//!
//! The crate wraps the Luau `Analysis` frontend through a C shim. A
//! [`crate::analyzer::Checker`]
//! loads host definitions once, then type-checks any number of sources and
//! returns structured [`crate::analyzer::Diagnostic`]s. Checkers persist their definitions
//! across calls and are `Send` but not `Sync`.
//!
//! [`crate::analyzer::Checker::check`] takes a source string.
//! [`crate::analyzer::Checker::check_path`] resolves
//! relative `require(...)` calls against the file's directory. Host-provided
//! in-memory modules flow through [`crate::analyzer::CheckOptions::virtual_modules`].
//!
//! # Example
//!
//! ```no_run
//! use ruau::analyzer::Checker;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let mut checker = Checker::new()?;
//! checker.add_definitions(
//!     r#"
//!     declare class TodoBuilder
//!         function content(self, content: string): TodoBuilder
//!     end
//!     declare Todo: { create: () -> TodoBuilder }
//!     "#,
//! )?;
//!
//! let result = checker.check(
//!     r#"
//!     --!strict
//!     local _todo = Todo.create():content("review")
//!     "#,
//! ).await?;
//! assert!(result.is_ok());
//! # Ok(())
//! # }
//! ```

use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    fs,
    marker::PhantomData,
    path::Path,
    ptr,
    rc::Rc,
    slice,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::Duration,
};

use thiserror::Error;

use crate::{
    Chunk, Luau,
    resolver::{ModuleId, ModuleResolveError, ResolverSnapshot, SourceSpan},
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
    /// Module that produced this diagnostic.
    pub module: ModuleId,
    /// Source span for this diagnostic.
    pub span: SourceSpan,
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

/// Aggregated declaration schema extracted from a `.d.luau` module manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModuleSchema {
    /// Top-level declared module-root global, if any.
    pub root: Option<ModuleRoot>,
    /// `declare class` declarations.
    pub classes: BTreeMap<String, ClassSchema>,
}

/// Top-level `declare <name>: { ... }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRoot {
    /// Global name declared by the module.
    pub name: String,
    /// Function and namespace shape rooted at the module table.
    pub namespace: NamespaceSchema,
}

/// One namespace level: function names plus nested child namespaces.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NamespaceSchema {
    /// Function-typed members callable directly at this level.
    pub functions: Vec<String>,
    /// Nested namespace members, name to schema.
    pub children: BTreeMap<String, Self>,
}

/// Method names declared inside a `declare class ... end` block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassSchema {
    /// Method names.
    pub methods: Vec<String>,
}

impl CheckResult {
    /// Returns `true` when the result completed and contains no errors.
    pub fn is_ok(&self) -> bool {
        !self.timed_out && !self.cancelled && !self.has_errors()
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
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics_with_severity(Severity::Error)
    }

    /// Returns all warning diagnostics.
    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics_with_severity(Severity::Warning)
    }

    /// Returns all diagnostics matching the requested severity.
    fn diagnostics_with_severity(&self, severity: Severity) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics
            .iter()
            .filter(move |diagnostic| diagnostic.severity == severity)
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

/// Errors returned by checker construction, source checking, and definition loading.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum AnalysisError {
    /// The native layer failed to create the checker.
    #[error("failed to create Luau checker")]
    CreateCheckerFailed,
    /// The native layer failed to create the cancellation token.
    #[error("failed to create Luau cancellation token")]
    CreateCancellationTokenFailed,
    /// Definitions failed to parse or type-check.
    #[error("failed to load Luau definitions: {0}")]
    Definitions(String),
    /// A checked load was rejected by analyzer diagnostics.
    #[error("checked load failed with {} diagnostic(s)", .0.diagnostics.len())]
    CheckFailed(CheckResult),
    /// A checked-load snapshot did not contain its root module.
    #[error("resolver snapshot is missing root module `{0}`")]
    MissingSnapshotRoot(String),
    /// A checked-load resolver failed to resolve a module.
    #[error("module resolution failed: {0}")]
    Resolve(#[from] ModuleResolveError),
    /// Checked-load runtime setup failed after analysis passed.
    #[error("failed to prepare checked load: {0}")]
    Load(String),
    /// Entrypoint schema extraction failed.
    #[error("failed to extract Luau entrypoint schema: {0}")]
    EntrypointSchema(String),
    /// Module declaration schema extraction failed.
    #[error("failed to extract Luau module schema: {0}")]
    ModuleSchema(String),
    /// Failed to read a UTF-8 text file for checking or definition loading.
    #[error("failed to read {kind} `{path}`: {message}")]
    ReadFile {
        /// Logical input category such as `"source"` or `"definitions"`.
        kind: &'static str,
        /// Display label for the file path.
        path: String,
        /// Human-readable I/O error message.
        message: String,
    },
    /// Checker input is too large for the C ABI length type.
    #[error("{kind} input is too large for checker FFI boundary ({len})")]
    InputTooLarge {
        /// Logical input category such as `"source"` or `"definitions"`.
        kind: &'static str,
        /// Original input byte length or item count.
        len: usize,
    },
    /// A previous async check is still draining on the blocking pool.
    ///
    /// The native checker handle is exclusive and the async API only allows one in-flight
    /// `check*` per `Checker`. Wait for the previous future to fully complete or drop the
    /// `Checker` to retry.
    #[error("checker is busy with a previous in-flight check")]
    Busy,
    /// The blocking analysis task panicked or was cancelled by the runtime.
    #[error("blocking analysis task failed: {0}")]
    BlockingTask(String),
}

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
    pub fn new() -> Result<Self, AnalysisError> {
        // SAFETY: Calling into shim constructor. Null indicates failure.
        let raw = unsafe { ffi::ruau_cancellation_token_new() };
        if raw.is_null() {
            return Err(AnalysisError::CreateCancellationTokenFailed);
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

/// Native checker handle plus the in-flight busy flag.
///
/// Wrapping the native handle in an `Arc` lets the `spawn_blocking` closure outlive the user's
/// `&mut Checker` borrow: when the future is dropped before the closure finishes, the closure's
/// `Arc` clone keeps the handle alive until the C call returns. The `busy` flag prevents the
/// next operation from re-entering the same handle while a previous job is still draining.
struct CheckerHandleInner {
    /// Opaque native checker handle. Freed in `Drop`.
    raw: ffi::RuauCheckerHandle,
    /// Set while a check is running on the blocking pool. `compare_exchange` claims the slot.
    busy: AtomicBool,
}

// SAFETY: The native checker is single-threaded for its operations, but the *handle* itself is
// just an opaque pointer and can move between threads. The busy flag and `Arc` together
// serialize access so only one operation touches the handle at a time.
unsafe impl Send for CheckerHandleInner {}
unsafe impl Sync for CheckerHandleInner {}

impl Drop for CheckerHandleInner {
    fn drop(&mut self) {
        // SAFETY: `raw` originates from `ruau_checker_new` and is valid until drop.
        unsafe { ffi::ruau_checker_free(self.raw) };
    }
}

/// RAII guard that clears the `busy` flag on drop, including the panic path.
struct BusyGuard(Arc<CheckerHandleInner>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, AtomicOrdering::Release);
    }
}

/// Synchronously-claimed busy slot that releases on drop unless transferred via `into_arc`.
///
/// Lets `check_with_options` hold the busy flag across fallible setup work (input copy,
/// token allocation), and then move ownership into the `spawn_blocking` closure. Failure
/// before transfer drops the claim and clears the flag automatically.
struct BusyClaim {
    handle: Arc<CheckerHandleInner>,
    armed: bool,
}

impl BusyClaim {
    fn new(handle: Arc<CheckerHandleInner>) -> Result<Self, AnalysisError> {
        handle
            .busy
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .map_err(|_| AnalysisError::Busy)?;
        Ok(Self {
            handle,
            armed: true,
        })
    }

    /// Transfers the busy flag to the caller. The claim is disarmed; the caller is now
    /// responsible for clearing the flag (typically by constructing a `BusyGuard`).
    fn into_arc(mut self) -> Arc<CheckerHandleInner> {
        self.armed = false;
        Arc::clone(&self.handle)
    }
}

impl Drop for BusyClaim {
    fn drop(&mut self) {
        if self.armed {
            self.handle.busy.store(false, AtomicOrdering::Release);
        }
    }
}

/// RAII guard that signals a `CancellationToken` on drop unless `disarm()`-ed first.
///
/// Used to cancel the native check when the async future is dropped (e.g. by
/// `tokio::time::timeout` or `select!`) without forcing callers to thread their own token.
/// Successful completion calls `disarm()` so caller-supplied reusable tokens stay clean.
struct CancelOnDrop {
    token: CancellationToken,
    armed: bool,
}

impl CancelOnDrop {
    fn armed(token: CancellationToken) -> Self {
        Self { token, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

/// Reusable checker instance with persistent global definitions.
///
/// `Checker` is `Send` but not `Sync`. The underlying Luau Analysis handle is moved into a
/// blocking thread pool by [`Checker::check`] family methods and is shared with that thread
/// through an internal `Arc`, so dropping a future while a check is still running is sound.
pub struct Checker {
    /// Shared handle that survives until the last `Arc` clone (caller or background closure).
    handle: Arc<CheckerHandleInner>,
    /// Default checker behavior options.
    options: CheckerOptions,
}

impl Checker {
    /// Creates a checker with default options.
    pub fn new() -> Result<Self, AnalysisError> {
        Self::with_options(CheckerOptions::default())
    }

    /// Creates a checker with explicit defaults.
    pub fn with_options(options: CheckerOptions) -> Result<Self, AnalysisError> {
        // SAFETY: Calling into shim constructor. Null indicates failure.
        let raw = unsafe { ffi::ruau_checker_new() };
        if raw.is_null() {
            return Err(AnalysisError::CreateCheckerFailed);
        }
        Ok(Self {
            handle: Arc::new(CheckerHandleInner {
                raw,
                busy: AtomicBool::new(false),
            }),
            options,
        })
    }

    /// Returns the checker's default options.
    pub fn options(&self) -> &CheckerOptions {
        &self.options
    }

    /// Loads Luau definition source using the default module label.
    pub fn add_definitions(&mut self, defs: &str) -> Result<(), AnalysisError> {
        let _busy = BusyClaim::new(Arc::clone(&self.handle))?;
        add_definitions_raw(
            self.handle.raw,
            defs,
            &self.options.default_definitions_module_name,
        )
    }

    /// Loads Luau definitions from a UTF-8 text file using the path as module label.
    ///
    /// This is a synchronous setup helper and performs a blocking filesystem read.
    pub fn add_definitions_path(&mut self, path: &Path) -> Result<(), AnalysisError> {
        let defs = LoadedInput::read(path, "definitions")?;
        let _busy = BusyClaim::new(Arc::clone(&self.handle))?;
        prefix_definitions_error(
            &defs.label,
            add_definitions_raw(self.handle.raw, &defs.contents, &defs.label),
        )
    }

    /// Loads Luau definition source with an explicit module label.
    pub fn add_definitions_with_name(
        &mut self,
        defs: &str,
        module_name: &str,
    ) -> Result<(), AnalysisError> {
        let _busy = BusyClaim::new(Arc::clone(&self.handle))?;
        add_definitions_raw(self.handle.raw, defs, module_name)
    }

    /// Type-checks a Luau source module with default options.
    pub async fn check(&mut self, source: &str) -> Result<CheckResult, AnalysisError> {
        self.check_with_options(source, CheckOptions::default())
            .await
    }

    /// Type-checks a Luau source file with default options and the path as module label.
    pub async fn check_path(&mut self, path: &Path) -> Result<CheckResult, AnalysisError> {
        self.check_path_with_options(path, CheckOptions::default())
            .await
    }

    /// Type-checks a Luau source file with explicit per-call options.
    ///
    /// Relative `require(...)` calls resolve against the file path unless
    /// `options.module_name` supplies a different module label.
    pub async fn check_path_with_options(
        &mut self,
        path: &Path,
        options: CheckOptions<'_>,
    ) -> Result<CheckResult, AnalysisError> {
        let path = path.to_path_buf();
        let label = path.display().to_string();
        let source = tokio::task::spawn_blocking(move || LoadedInput::read(&path, "source"))
            .await
            .map_err(|error| AnalysisError::ReadFile {
                kind: "source",
                path: label,
                message: error.to_string(),
            })??;
        self.check_with_options(
            &source.contents,
            options.with_fallback_module_name(source.label.as_str()),
        )
        .await
    }

    /// Type-checks a pre-resolved module graph.
    pub async fn check_snapshot(
        &mut self,
        snapshot: &ResolverSnapshot,
    ) -> Result<CheckResult, AnalysisError> {
        let root = snapshot
            .root_source()
            .ok_or_else(|| AnalysisError::MissingSnapshotRoot(snapshot.root().to_string()))?;
        let virtual_modules = snapshot.virtual_modules();
        self.check_with_options(
            root.source(),
            CheckOptions {
                module_name: Some(root.id().as_str()),
                virtual_modules: &virtual_modules,
                ..CheckOptions::default()
            },
        )
        .await
    }

    /// Type-checks a Luau source module with explicit per-call options.
    ///
    /// The native checker runs on the Tokio blocking pool so the executor thread stays free.
    /// If the returned future is dropped (e.g. by `tokio::time::timeout`), an internal drop
    /// guard signals the native cancellation token. Caller-supplied tokens stay reusable —
    /// only the in-flight check is affected.
    pub async fn check_with_options(
        &mut self,
        source: &str,
        options: CheckOptions<'_>,
    ) -> Result<CheckResult, AnalysisError> {
        let claim = BusyClaim::new(Arc::clone(&self.handle))?;
        let owned = OwnedCheckInputs::from_borrowed(source, &options, &self.options)?;

        // Always operate against a token so we can cancel-on-drop. Caller tokens are cloned;
        // otherwise create a fresh native token for this check.
        let token = match options.cancellation_token {
            Some(t) => t.clone(),
            None => CancellationToken::new()?,
        };
        let mut guard = CancelOnDrop::armed(token.clone());

        // Hand the busy flag to the closure: it now owns the slot and clears it on drop.
        let handle = claim.into_arc();
        let weak_handle = Arc::clone(&handle);

        let join = tokio::task::spawn_blocking(move || -> Result<CheckResult, AnalysisError> {
            let _busy = BusyGuard(Arc::clone(&handle));
            let raw_options = owned.as_ffi(token.raw());
            // SAFETY: `handle.raw` is kept alive by this Arc clone for the duration of the
            // call. The owned input pointers come from `owned` which lives for the closure.
            let raw = unsafe {
                ffi::ruau_checker_check(
                    handle.raw,
                    owned.source_ptr(),
                    owned.source_len(),
                    &raw_options,
                )
            };
            let raw_guard = RawGuard::new(raw);
            let raw_ref = raw_guard.as_ref();
            let mut diagnostics = collect_diagnostics(raw_ref, &owned.module_id);
            diagnostics.sort_by(diagnostic_sort_key);
            Ok(CheckResult {
                diagnostics,
                timed_out: raw_ref.timed_out != 0,
                cancelled: raw_ref.cancelled != 0,
            })
        });

        let result = match join.await {
            Ok(result) => result,
            Err(err) => {
                // The blocking task panicked or the runtime is shutting down. Defensively
                // clear the busy flag in case the closure never ran (the closure clears it
                // itself on the panic path through `BusyGuard::drop`).
                weak_handle.busy.store(false, AtomicOrdering::Release);
                return Err(AnalysisError::BlockingTask(err.to_string()));
            }
        }?;

        guard.disarm();
        Ok(result)
    }
}

impl Luau {
    /// Type-checks a resolver snapshot before returning a loadable root chunk.
    pub async fn checked_load(
        &self,
        checker: &mut Checker,
        snapshot: ResolverSnapshot,
    ) -> Result<Chunk<'static>, AnalysisError> {
        let result = checker.check_snapshot(&snapshot).await?;
        if !result.is_ok() {
            return Err(AnalysisError::CheckFailed(result));
        }

        let root = snapshot
            .root_source()
            .ok_or_else(|| AnalysisError::MissingSnapshotRoot(snapshot.root().to_string()))?;
        let root_id = root.id().clone();
        let root_source = root.source().to_owned();

        // Reuse the runtime resolver→`require` plumbing: ResolverSnapshot itself implements
        // ModuleResolver, so the same builder serves both checked load and live require.
        let resolver: crate::runtime::require::SharedResolver = Rc::new(snapshot);
        let cache: crate::runtime::require::RuntimeModuleCache =
            Rc::new(RefCell::new(HashMap::new()));
        let env = crate::runtime::require::resolver_environment(
            self,
            resolver,
            cache,
            Some(root_id.clone()),
        )
        .map_err(|error| AnalysisError::Load(error.to_string()))?;

        Ok(self
            .load(root_source)
            .name(root_id.as_str())
            .environment(env))
    }

    /// Resolves, type-checks, and loads a root module in one step.
    pub async fn checked_load_resolved<R>(
        &self,
        checker: &mut Checker,
        resolver: &R,
        root: impl Into<ModuleId>,
    ) -> Result<Chunk<'static>, AnalysisError>
    where
        R: crate::resolver::ModuleResolver + ?Sized,
    {
        let snapshot = ResolverSnapshot::resolve(resolver, root).await?;
        self.checked_load(checker, snapshot).await
    }
}

/// Extracts parameter names, annotation text, and optionality from a direct
/// `return function(...) ... end` chunk.
pub fn extract_entrypoint_schema(source: &str) -> Result<EntrypointSchema, AnalysisError> {
    let source = FfiStr::new(source, "source")?;
    // SAFETY: Input pointer is valid for the call duration.
    let raw = unsafe { ffi::ruau_extract_entrypoint_schema(source.ptr(), source.len()) };
    let raw = RawGuard::new(raw);

    if raw.as_ref().error_len != 0 {
        return Err(AnalysisError::EntrypointSchema(string_from_raw(
            raw.as_ref().error,
            raw.as_ref().error_len,
        )));
    }

    Ok(EntrypointSchema {
        params: collect_entrypoint_params(raw.as_ref()),
    })
}

/// Extracts the top-level module declaration and class method declarations from `.d.luau` source.
pub fn extract_module_schema(source: &str) -> Result<ModuleSchema, AnalysisError> {
    let stripped = strip_comments(source);
    let mut schema = ModuleSchema::default();
    let mut cursor = stripped.as_str();

    while let Some(declare_at) = next_top_level_declare(cursor) {
        cursor = &cursor[declare_at + "declare ".len()..];
        cursor = trim_start(cursor);

        if let Some(after) = cursor.strip_prefix("class ") {
            let (name, body, rest) = read_class_block(after)?;
            schema
                .classes
                .insert(name.to_owned(), parse_class_body(body));
            cursor = rest;
            continue;
        }

        if let Some((name, after_name)) = read_identifier(cursor) {
            let after_colon = trim_start(after_name);
            if let Some(after_colon) = after_colon.strip_prefix(':') {
                let (namespace, rest) = parse_namespace_type(trim_start(after_colon))?;
                if let Some(existing) = schema.root.as_ref() {
                    return Err(AnalysisError::ModuleSchema(format!(
                        "multiple module-root declarations: `{}` and `{name}`",
                        existing.name
                    )));
                }
                schema.root = Some(ModuleRoot {
                    name: name.to_owned(),
                    namespace,
                });
                cursor = rest;
            } else {
                cursor = after_name;
            }
            continue;
        }

        cursor = skip_to_newline(cursor);
    }

    Ok(schema)
}

fn next_top_level_declare(source: &str) -> Option<usize> {
    let mut at_line_start = true;
    for (index, character) in source.char_indices() {
        if at_line_start && source[index..].starts_with("declare ") {
            return Some(index);
        }
        at_line_start = character == '\n';
    }
    None
}

fn read_class_block(source: &str) -> Result<(&str, &str, &str), AnalysisError> {
    let (name, after_name) =
        read_identifier(source).ok_or_else(|| module_schema_error("expected class name"))?;
    let mut cursor = trim_start(after_name);

    if let Some(rest) = cursor.strip_prefix("extends ") {
        let after_extends = trim_start(rest);
        let (_parent, after_parent) = read_identifier(after_extends)
            .ok_or_else(|| module_schema_error("expected parent class name"))?;
        cursor = trim_start(after_parent);
    }

    let body_start = cursor;
    let end_offset = find_keyword_end(cursor)
        .ok_or_else(|| module_schema_error(format!("class `{name}` is missing `end`")))?;
    let body = &body_start[..end_offset];
    let rest = &body_start[end_offset + "end".len()..];

    Ok((name, body, rest))
}

fn parse_class_body(body: &str) -> ClassSchema {
    let mut methods = Vec::new();
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }

        if let Some(rest) = line.strip_prefix("function ")
            && let Some((name, after)) = read_identifier(rest)
            && trim_start(after).starts_with('(')
        {
            methods.push(name.to_owned());
            continue;
        }

        if let Some((name, after)) = read_identifier(line) {
            let trimmed = trim_start(after);
            if let Some(rest) = trimmed.strip_prefix(':')
                && trim_start(rest).starts_with('(')
            {
                methods.push(name.to_owned());
            }
        }
    }
    ClassSchema { methods }
}

fn parse_namespace_type(source: &str) -> Result<(NamespaceSchema, &str), AnalysisError> {
    let source = trim_start(source);
    let after_brace = source
        .strip_prefix('{')
        .ok_or_else(|| module_schema_error("expected `{`"))?;

    let close_at = find_matching_brace(after_brace)?;
    let inside = &after_brace[..close_at];
    let rest = &after_brace[close_at + 1..];

    Ok((parse_namespace_body(inside)?, rest))
}

fn parse_namespace_body(body: &str) -> Result<NamespaceSchema, AnalysisError> {
    let mut namespace = NamespaceSchema::default();
    for entry in split_top_level_commas(body) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }

        let (key, after_key) = read_identifier(trimmed)
            .ok_or_else(|| module_schema_error(format!("missing key: `{trimmed}`")))?;
        let after_colon = trim_start(after_key);
        let value = after_colon
            .strip_prefix(':')
            .ok_or_else(|| module_schema_error(format!("missing `:` after `{key}`")))?
            .trim();

        if value.starts_with('(') {
            namespace.functions.push(key.to_owned());
        } else if value.starts_with('{') {
            let (child, _) = parse_namespace_type(value)?;
            namespace.children.insert(key.to_owned(), child);
        }
    }
    Ok(namespace)
}

fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + 1 < bytes.len() && bytes[index] == b'-' && bytes[index + 1] == b'-' {
            if index + 3 < bytes.len() && bytes[index + 2] == b'[' && bytes[index + 3] == b'[' {
                index += 4;
                while index + 1 < bytes.len() && !(bytes[index] == b']' && bytes[index + 1] == b']')
                {
                    output.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
                    index += 1;
                }
                if index + 1 < bytes.len() {
                    index += 2;
                }
                continue;
            }

            while index < bytes.len() && bytes[index] != b'\n' {
                output.push(b' ');
                index += 1;
            }
            continue;
        }

        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_default()
}

fn read_identifier(source: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for (index, character) in source.char_indices() {
        let ok = if index == 0 {
            character == '_' || character.is_ascii_alphabetic()
        } else {
            character == '_' || character.is_ascii_alphanumeric()
        };
        if ok {
            end = index + character.len_utf8();
        } else {
            break;
        }
    }

    (end != 0).then_some((&source[..end], &source[end..]))
}

fn trim_start(source: &str) -> &str {
    source.trim_start()
}

fn skip_to_newline(source: &str) -> &str {
    if let Some(index) = source.find('\n') {
        &source[index + 1..]
    } else {
        ""
    }
}

fn find_matching_brace(source: &str) -> Result<usize, AnalysisError> {
    let bytes = source.as_bytes();
    let mut depth = 1_i32;
    let mut paren_depth = 0_i32;
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index);
                }
            }
            b'(' => paren_depth += 1,
            b')' => paren_depth -= 1,
            _ => {}
        }
        if depth < 0 || paren_depth < 0 {
            return Err(module_schema_error(
                "unbalanced punctuation in namespace body",
            ));
        }
    }
    Err(module_schema_error("unterminated namespace body"))
}

fn split_top_level_commas(body: &str) -> Vec<&str> {
    let mut output = Vec::new();
    let mut depth = 0_i32;
    let mut start = 0;
    for (index, character) in body.char_indices() {
        match character {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                output.push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    output.push(&body[start..]);
    output
}

fn find_keyword_end(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        if &bytes[index..index + 3] == b"end" {
            let before_ok = index == 0 || !is_ident_byte(bytes[index - 1]);
            let after_ok = index + 3 == bytes.len() || !is_ident_byte(bytes[index + 3]);
            if before_ok && after_ok {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn module_schema_error(message: impl Into<String>) -> AnalysisError {
    AnalysisError::ModuleSchema(message.into())
}

/// Loads Luau definition source through the native checker with a chosen module label.
fn add_definitions_raw(
    checker: ffi::RuauCheckerHandle,
    defs: &str,
    module_name: &str,
) -> Result<(), AnalysisError> {
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
        Err(AnalysisError::Definitions(string_from_raw(
            string.data,
            string.len,
        )))
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
    fn read(path: &Path, kind: &'static str) -> Result<Self, AnalysisError> {
        let label = path.display().to_string();
        let contents = fs::read_to_string(path).map_err(|error| AnalysisError::ReadFile {
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
    fn new(value: &'a str, kind: &'static str) -> Result<Self, AnalysisError> {
        let len = u32::try_from(value.len()).map_err(|_| AnalysisError::InputTooLarge {
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

/// One owned virtual module's stable backing storage.
///
/// `name` and `source` are heap-allocated `Box<[u8]>`, so their pointers stay stable across
/// moves of the enclosing `OwnedCheckInputs`.
struct OwnedVirtualModule {
    name: Box<[u8]>,
    name_len: u32,
    source: Box<[u8]>,
    source_len: u32,
}

/// Owned, `Send + 'static` package of inputs for one `ruau_checker_check` call.
///
/// Built on the runtime thread before `tokio::task::spawn_blocking` is invoked. All input
/// pointers passed to the C ABI are derived from boxed slices owned by this struct, so the
/// data outlives any caller borrows for the duration of the blocking work.
///
/// `virtual_module_entries` contains raw pointers into the boxed slices in
/// `virtual_module_storage`; both fields move together with the struct since the heap
/// allocations are pointed-at by stable addresses.
struct OwnedCheckInputs {
    module_id: ModuleId,
    source: Box<[u8]>,
    source_len: u32,
    module_name: Box<[u8]>,
    module_name_len: u32,
    timeout: Option<Duration>,
    /// Stable backing storage; pointers in `virtual_module_entries` reference these boxes.
    #[allow(
        dead_code,
        reason = "kept alive so `virtual_module_entries` pointers stay valid"
    )]
    virtual_module_storage: Vec<OwnedVirtualModule>,
    virtual_module_entries: Vec<ffi::RuauVirtualModule>,
}

// SAFETY: All pointer fields point into heap-allocated `Box<[u8]>` storage that is kept alive
// by the same struct. Moving the struct does not invalidate those pointers.
unsafe impl Send for OwnedCheckInputs {}

impl OwnedCheckInputs {
    fn from_borrowed(
        source: &str,
        options: &CheckOptions<'_>,
        defaults: &CheckerOptions,
    ) -> Result<Self, AnalysisError> {
        let source_len = u32::try_from(source.len()).map_err(|_| AnalysisError::InputTooLarge {
            kind: "source",
            len: source.len(),
        })?;
        let module_name = options
            .module_name
            .unwrap_or(defaults.default_module_name.as_str());
        let module_name_len =
            u32::try_from(module_name.len()).map_err(|_| AnalysisError::InputTooLarge {
                kind: "module name",
                len: module_name.len(),
            })?;

        let mut virtual_module_storage = Vec::with_capacity(options.virtual_modules.len());
        for module in options.virtual_modules {
            let name_len =
                u32::try_from(module.name.len()).map_err(|_| AnalysisError::InputTooLarge {
                    kind: "virtual module name",
                    len: module.name.len(),
                })?;
            let source_len =
                u32::try_from(module.source.len()).map_err(|_| AnalysisError::InputTooLarge {
                    kind: "virtual module source",
                    len: module.source.len(),
                })?;
            virtual_module_storage.push(OwnedVirtualModule {
                name: module.name.as_bytes().to_vec().into_boxed_slice(),
                name_len,
                source: module.source.as_bytes().to_vec().into_boxed_slice(),
                source_len,
            });
        }
        let _: u32 = u32::try_from(virtual_module_storage.len()).map_err(|_| {
            AnalysisError::InputTooLarge {
                kind: "virtual modules",
                len: virtual_module_storage.len(),
            }
        })?;

        // Build the FFI entry array with pointers into the heap-stable storage above.
        let virtual_module_entries: Vec<ffi::RuauVirtualModule> = virtual_module_storage
            .iter()
            .map(|m| ffi::RuauVirtualModule {
                name: if m.name.is_empty() {
                    ptr::null()
                } else {
                    m.name.as_ptr()
                },
                name_len: m.name_len,
                source: if m.source.is_empty() {
                    ptr::null()
                } else {
                    m.source.as_ptr()
                },
                source_len: m.source_len,
            })
            .collect();

        Ok(Self {
            module_id: ModuleId::new(module_name),
            source: source.as_bytes().to_vec().into_boxed_slice(),
            source_len,
            module_name: module_name.as_bytes().to_vec().into_boxed_slice(),
            module_name_len,
            timeout: options.timeout.or(defaults.default_timeout),
            virtual_module_storage,
            virtual_module_entries,
        })
    }

    fn source_ptr(&self) -> *const u8 {
        if self.source.is_empty() {
            ptr::null()
        } else {
            self.source.as_ptr()
        }
    }

    fn source_len(&self) -> u32 {
        self.source_len
    }

    /// Builds the raw `RuauCheckOptions` value pointing into this struct's owned data.
    ///
    /// The returned struct borrows from `self`; it must not outlive `self`. The
    /// `cancellation_token` argument is the C handle obtained from a live `CancellationToken`
    /// kept alive by the caller for at least the same duration.
    fn as_ffi(&self, cancellation_token: ffi::RuauTokenHandle) -> ffi::RuauCheckOptions {
        ffi::RuauCheckOptions {
            module_name: if self.module_name.is_empty() {
                ptr::null()
            } else {
                self.module_name.as_ptr()
            },
            module_name_len: self.module_name_len,
            has_timeout: u32::from(self.timeout.is_some()),
            timeout_seconds: self.timeout.map_or(0.0, |duration| duration.as_secs_f64()),
            cancellation_token,
            virtual_modules: if self.virtual_module_entries.is_empty() {
                ptr::null()
            } else {
                self.virtual_module_entries.as_ptr()
            },
            virtual_module_count: self.virtual_module_entries.len() as u32,
        }
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
fn prefix_definitions_error(
    label: &str,
    result: Result<(), AnalysisError>,
) -> Result<(), AnalysisError> {
    match result {
        Err(AnalysisError::Definitions(message)) => {
            Err(AnalysisError::Definitions(format!("{label}: {message}")))
        }
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
fn collect_diagnostics(raw: &ffi::RuauCheckResult, module: &ModuleId) -> Vec<Diagnostic> {
    // SAFETY: `raw.diagnostics` points to `diagnostic_count` entries owned by `raw`.
    unsafe { raw_slice(raw.diagnostics, raw.diagnostic_count) }
        .iter()
        .map(|diagnostic| Diagnostic {
            module: module.clone(),
            span: SourceSpan {
                line: diagnostic.line,
                column: diagnostic.col,
                end_line: diagnostic.end_line,
                end_column: diagnostic.end_col,
            },
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
    left.module
        .cmp(&right.module)
        .then(left.span.cmp(&right.span))
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
        AnalysisError, CheckResult, Checker, CheckerOptions, Diagnostic, Severity,
        extract_entrypoint_schema, extract_module_schema,
    };
    use crate::resolver::{ModuleId, SourceSpan};

    /// Verifies `CheckResult::is_ok` is true for warning-only results.
    #[test]
    fn check_result_ok_with_warnings() {
        let result = CheckResult {
            diagnostics: vec![Diagnostic {
                module: ModuleId::new("test"),
                span: SourceSpan {
                    line: 0,
                    column: 0,
                    end_line: 0,
                    end_column: 1,
                },
                severity: Severity::Warning,
                message: "unused local".to_owned(),
            }],
            timed_out: false,
            cancelled: false,
        };

        assert!(result.is_ok());
        assert_eq!(1, result.warnings().count());
        assert_eq!(0, result.errors().count());
    }

    /// Verifies `CheckResult::is_ok` is false when at least one error exists.
    #[test]
    fn check_result_not_ok_with_error() {
        let result = CheckResult {
            diagnostics: vec![Diagnostic {
                module: ModuleId::new("test"),
                span: SourceSpan {
                    line: 1,
                    column: 1,
                    end_line: 1,
                    end_column: 5,
                },
                severity: Severity::Error,
                message: "type mismatch".to_owned(),
            }],
            timed_out: false,
            cancelled: false,
        };

        assert!(!result.is_ok());
        assert_eq!(0, result.warnings().count());
        assert_eq!(1, result.errors().count());
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

    /// Verifies module schema extraction reads module roots, namespaces, and class methods.
    #[test]
    fn extract_module_schema_reads_root_and_classes() {
        let schema = extract_module_schema(
            r#"
export type Mode = "read" | "write"

declare class Store
    field: string
    function get(self, key: string): string?
    put: (self, key: string, value: string) -> ()
end

declare demo: {
    open: (name: string) -> Store,
    nested: {
        count: () -> number,
    },
}
"#,
        )
        .expect("schema");

        let root = schema.root.expect("module root");
        assert_eq!("demo", root.name);
        assert_eq!(vec!["open"], root.namespace.functions);
        assert_eq!(vec!["count"], root.namespace.children["nested"].functions);
        assert_eq!(vec!["get", "put"], schema.classes["Store"].methods);
    }

    /// Verifies module schema extraction rejects multiple module roots.
    #[test]
    fn extract_module_schema_rejects_multiple_roots() {
        let error = extract_module_schema(
            r#"
declare first: {}
declare second: {}
"#,
        )
        .expect_err("schema should fail");

        assert!(
            error
                .to_string()
                .contains("multiple module-root declarations"),
            "{error}"
        );
    }

    /// Verifies path-based source checks surface readable file errors.
    #[tokio::test]
    async fn check_path_reports_read_error() {
        let mut checker = Checker::new().expect("checker creation should succeed");
        let missing = temp_path("missing_source");

        let error = checker
            .check_path(&missing)
            .await
            .expect_err("missing file should fail");
        match error {
            AnalysisError::ReadFile {
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
    #[tokio::test]
    async fn add_definitions_path_loads_file_contents() {
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
            .await
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
