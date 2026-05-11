//! In-process Luau type checking for Rust.
//!
//! The crate wraps the Luau `Analysis` frontend through a C shim. A
//! [`crate::analyzer::Checker`]
//! loads host definitions once, then type-checks any number of sources and
//! returns structured [`crate::analyzer::Diagnostic`]s. Checkers persist their definitions
//! across calls and are `Send` but not `Sync`.
//! Reuse a checker sequentially: await one `check*` future to completion before starting the
//! next. A concurrent attempt on the same checker returns [`crate::analyzer::AnalysisError::Busy`].
//! [`crate::analyzer::ModuleInterfaceSet`] is an immutable, cheap-to-clone collection of virtual
//! modules for host-managed APIs; `check_with_interfaces` reads it without mutation, so embedders
//! can keep one set per session and replace it only when the host catalog changes.
//!
//! [`crate::analyzer::Checker::check`] takes a source string.
//! [`crate::analyzer::Checker::check_path`] resolves
//! relative `require(...)` calls against the file's directory. Host-provided
//! in-memory modules flow through [`crate::analyzer::CheckOptions::virtual_modules`].
//! Checked runtime loading follows the static require policy documented in
//! [`crate::resolver`]: resolver snapshots include only direct string-literal `require(...)`
//! calls. Unsupported dynamic require expressions are rejected by analysis instead of being added
//! to the runtime snapshot.
//! For host catalogs, insert declaration `.d.luau` sources with
//! [`crate::analyzer::ModuleInterfaceSet::insert`], insert implementation `.luau` modules with
//! [`crate::analyzer::ModuleInterfaceSet::insert_implementation`], check scripts with
//! [`crate::analyzer::Checker::check_with_interfaces`], and load runtime source with
//! [`crate::Luau::checked_load`] or [`crate::Luau::checked_load_resolved`] so analysis and runtime
//! see the same module graph.
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
//!
//! # Interface set example
//!
//! ```no_run
//! use ruau::{Luau, analyzer::{Checker, ModuleInterfaceSet}, resolver::InMemoryResolver};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let mut interfaces = ModuleInterfaceSet::new();
//! interfaces.insert("fs", "export type Module = { read: (path: string) -> string }")?;
//! interfaces.insert_implementation("helpers", "return { label = function() return 'ok' end }");
//!
//! let mut checker = Checker::new()?;
//! let checked = checker.check_with_interfaces("local fs = require('fs')", &interfaces).await?;
//! assert!(checked.is_ok());
//!
//! let resolver = InMemoryResolver::new().with_module("main", "return require('helpers').label()");
//! let lua = Luau::new();
//! let value: String = lua.checked_load_resolved(&mut checker, &resolver, "main").await?.eval().await?;
//! assert_eq!("ok", value);
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
use tokio::task::spawn_blocking;

use crate::{
    Chunk, Luau,
    resolver::{
        ModuleId, ModuleResolveError, ModuleResolver, ModuleSource, ResolverSnapshot, SourceSpan,
    },
    runtime::require::{RuntimeModuleCache, SharedResolver, resolver_environment},
    util::shim::{FfiResource, RawGuard},
};

mod schema;
use schema::checker_source_for_interface;
pub use schema::extract_module_schema;

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
    /// Top-of-file module description comment, when present.
    pub module_description: Option<String>,
    /// Top-level declared module-root global, if any.
    pub root: Option<ModuleRoot>,
    /// `declare class` declarations.
    pub classes: BTreeMap<String, ClassSchema>,
    /// Exported type aliases preserved from source.
    pub type_aliases: BTreeMap<String, TypeAliasSchema>,
}

/// Top-level `declare <name>: { ... }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRoot {
    /// Global name declared by the module.
    pub name: String,
    /// Function and namespace shape rooted at the module table.
    pub namespace: NamespaceSchema,
    /// Source span for the root declaration when known.
    pub span: Option<SourceSpan>,
}

/// One namespace level: function names plus nested child namespaces.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NamespaceSchema {
    /// Function-typed members callable directly at this level.
    pub functions: Vec<String>,
    /// Callable signatures keyed by member name.
    pub callables: BTreeMap<String, CallableSchema>,
    /// Nested namespace members, name to schema.
    pub children: BTreeMap<String, Self>,
}

/// Method names declared inside a `declare class ... end` block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassSchema {
    /// Method names.
    pub methods: Vec<String>,
    /// Method signatures keyed by method name.
    pub method_signatures: BTreeMap<String, CallableSchema>,
    /// Non-method fields keyed by field name.
    pub fields: BTreeMap<String, FieldSchema>,
    /// Source span for the class declaration when known.
    pub span: Option<SourceSpan>,
}

/// Class field declaration extracted from source.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FieldSchema {
    /// Field name.
    pub name: String,
    /// Field type source slice.
    pub ty: TypeSlice,
    /// Source span for the field declaration when known.
    pub span: Option<SourceSpan>,
    /// Contiguous `--` doc comment immediately above the field, when present.
    pub docs: Option<String>,
}

/// Callable signature extracted from a declaration source.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallableSchema {
    /// Arguments in source order.
    pub args: Vec<ArgumentSchema>,
    /// Return type source slice.
    pub returns: TypeSlice,
    /// Whether the callable was declared as a method taking `self`.
    pub method: bool,
    /// Source span for the callable declaration when known.
    pub span: Option<SourceSpan>,
    /// Contiguous `--` doc comment immediately above the callable, when present.
    pub docs: Option<String>,
}

/// One callable argument.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArgumentSchema {
    /// Argument name.
    pub name: String,
    /// Argument type source slice.
    pub ty: TypeSlice,
    /// Whether the argument name used Luau optional syntax.
    pub optional: bool,
    /// Source span for the argument declaration when known.
    pub span: Option<SourceSpan>,
}

/// Opaque Luau type expression text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TypeSlice {
    /// Type expression as written, trimmed.
    pub source: String,
    /// Span of the type expression when known.
    pub span: Option<SourceSpan>,
}

/// Exported type alias source slice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TypeAliasSchema {
    /// Alias name.
    pub name: String,
    /// Alias body source slice.
    pub ty: TypeSlice,
    /// Full source text for the alias declaration.
    pub source: String,
    /// Source span for the alias when known.
    pub span: Option<SourceSpan>,
    /// Contiguous `--` doc comment immediately above the alias, when present.
    pub docs: Option<String>,
}

/// How an interface source contributes to type checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleInterfaceKind {
    /// `.d.luau` declaration source; `schema` is populated.
    #[default]
    Declaration,
    /// `.luau` implementation source; the analyzer infers the exported type.
    Implementation,
}

/// Parsed, reusable interface entry for one require specifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInterface {
    /// Require specifier used by scripts, for example `rust/cargo`.
    specifier: String,
    /// Original `.d.luau` source.
    source: String,
    /// Diagnostics produced while validating this interface.
    diagnostics: Vec<Diagnostic>,
    /// Rich schema extracted from the declaration source.
    schema: ModuleSchema,
    /// Generated normal Luau module source used by the checker.
    checker_source: String,
    /// Whether this interface can be required as a value.
    requireable: bool,
    /// Whether the interface is declaration or implementation source.
    kind: ModuleInterfaceKind,
}

impl ModuleInterface {
    /// Returns the require specifier used by scripts, for example `rust/cargo`.
    #[must_use]
    pub fn specifier(&self) -> &str {
        &self.specifier
    }

    /// Returns the original source used to create this interface.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns diagnostics produced while validating this interface.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Returns the schema extracted from declaration source.
    #[must_use]
    pub fn schema(&self) -> &ModuleSchema {
        &self.schema
    }

    /// Returns the checker-facing Luau module source generated for this interface.
    #[must_use]
    pub fn checker_source(&self) -> &str {
        &self.checker_source
    }

    /// Returns whether this interface can be required as a value.
    #[must_use]
    pub const fn is_requireable(&self) -> bool {
        self.requireable
    }

    /// Returns whether the interface is declaration or implementation source.
    #[must_use]
    pub const fn kind(&self) -> ModuleInterfaceKind {
        self.kind
    }

    /// Returns this interface as a checker virtual module.
    #[must_use]
    fn virtual_module(&self) -> VirtualModule<'_> {
        VirtualModule {
            name: &self.specifier,
            source: &self.checker_source,
        }
    }
}

/// Named collection of typed module interfaces.
///
/// The set is plain owned data: checks borrow it, build virtual modules from its current
/// contents, and do not mutate it. Embedders can keep one set for a session and clone or replace
/// it when their catalog changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleInterfaceSet {
    interfaces: BTreeMap<String, ModuleInterface>,
}

impl ModuleInterfaceSet {
    /// Creates an empty interface set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts one interface source and returns the previous entry, if any.
    pub fn insert(
        &mut self,
        specifier: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Option<ModuleInterface>, AnalysisError> {
        let specifier = specifier.into();
        let source = source.into();
        let schema = extract_module_schema(&source)?;
        let checker_source = checker_source_for_interface(&schema, &source)?;
        let requireable = schema.root.is_some();
        let interface = ModuleInterface {
            specifier: specifier.clone(),
            source,
            diagnostics: Vec::new(),
            schema,
            checker_source,
            requireable,
            kind: ModuleInterfaceKind::Declaration,
        };
        Ok(self.interfaces.insert(specifier, interface))
    }

    /// Inserts implementation source as a requireable virtual module.
    ///
    /// Implementation interfaces have no declaration schema. The checker infers their exported
    /// shape from the source each time the containing interface set is used.
    pub fn insert_implementation(
        &mut self,
        specifier: impl Into<String>,
        source: impl Into<String>,
    ) -> Option<ModuleInterface> {
        let specifier = specifier.into();
        let source = source.into();
        let interface = ModuleInterface {
            specifier: specifier.clone(),
            checker_source: source.clone(),
            source,
            diagnostics: Vec::new(),
            schema: ModuleSchema::default(),
            requireable: true,
            kind: ModuleInterfaceKind::Implementation,
        };
        self.interfaces.insert(specifier, interface)
    }

    /// Inserts a pre-resolved interface and returns the previous entry, if any.
    pub fn insert_interface(&mut self, interface: ModuleInterface) -> Option<ModuleInterface> {
        self.interfaces
            .insert(interface.specifier.clone(), interface)
    }

    /// Inserts a pre-resolved interface source.
    pub fn insert_source(
        &mut self,
        source: &ModuleSource,
    ) -> Result<Option<ModuleInterface>, AnalysisError> {
        self.insert(source.id().as_str().to_owned(), source.source().to_owned())
    }

    /// Inserts and validates one interface source with the supplied checker.
    pub async fn insert_checked(
        &mut self,
        checker: &mut Checker,
        specifier: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Option<ModuleInterface>, AnalysisError> {
        let specifier = specifier.into();
        let source = source.into();
        let schema = extract_module_schema(&source)?;
        let checker_source = checker_source_for_interface(&schema, &source)?;
        let requireable = schema.root.is_some();
        let result = checker
            .check_with_options(
                &checker_source,
                CheckOptions {
                    module_name: Some(specifier.as_str()),
                    ..CheckOptions::default()
                },
            )
            .await?;
        let interface = ModuleInterface {
            specifier: specifier.clone(),
            source,
            diagnostics: result.diagnostics,
            schema,
            checker_source,
            requireable,
            kind: ModuleInterfaceKind::Declaration,
        };
        Ok(self.interfaces.insert(specifier, interface))
    }

    /// Returns the interface for one specifier.
    #[must_use]
    pub fn get(&self, specifier: &str) -> Option<&ModuleInterface> {
        self.interfaces.get(specifier)
    }

    /// Iterates interfaces in stable specifier order.
    pub fn interfaces(&self) -> impl Iterator<Item = &ModuleInterface> {
        self.interfaces.values()
    }

    /// Returns checker virtual modules for requireable interfaces.
    #[must_use]
    pub fn virtual_modules(&self) -> Vec<VirtualModule<'_>> {
        self.interfaces
            .values()
            .filter(|interface| interface.requireable)
            .map(ModuleInterface::virtual_module)
            .collect()
    }
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

// SAFETY: The underlying C cancellation token uses atomic state and is thread-safe for
// signal/reset. The handle itself is an opaque pointer that can be moved or shared across
// threads.
unsafe impl Send for CancellationTokenInner {}
// SAFETY: see Send impl above.
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
// SAFETY: see Send impl above.
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
/// Start at most one check at a time per checker; a second in-flight call returns
/// [`AnalysisError::Busy`]. Await or cancel-and-drain the first future before reusing the checker.
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
        let source = spawn_blocking(move || LoadedInput::read(&path, "source"))
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
    ///
    /// The graph contains only the direct string-literal `require(...)` dependencies collected by
    /// [`ResolverSnapshot`]. Dynamic requires are not added as virtual modules by this method.
    pub async fn check_snapshot(
        &mut self,
        snapshot: &ResolverSnapshot,
    ) -> Result<CheckResult, AnalysisError> {
        self.check_snapshot_with_interfaces(snapshot, &ModuleInterfaceSet::new())
            .await
    }

    /// Type-checks a pre-resolved module graph against host interfaces.
    ///
    /// Runtime module source comes from the snapshot; declaration-only host modules should be
    /// passed through `interfaces` instead of returned by the resolver.
    pub async fn check_snapshot_with_interfaces(
        &mut self,
        snapshot: &ResolverSnapshot,
        interfaces: &ModuleInterfaceSet,
    ) -> Result<CheckResult, AnalysisError> {
        let root = snapshot
            .root_source()
            .ok_or_else(|| AnalysisError::MissingSnapshotRoot(snapshot.root().to_string()))?;
        let mut virtual_modules = snapshot.virtual_modules();
        virtual_modules.extend(interfaces.virtual_modules());
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

    /// Type-checks a Luau source module against a named module interface set.
    pub async fn check_with_interfaces(
        &mut self,
        source: &str,
        interfaces: &ModuleInterfaceSet,
    ) -> Result<CheckResult, AnalysisError> {
        self.check_with_interfaces_options(source, interfaces, CheckOptions::default())
            .await
    }

    /// Type-checks a Luau source module against interfaces plus explicit per-call options.
    pub async fn check_with_interfaces_options(
        &mut self,
        source: &str,
        interfaces: &ModuleInterfaceSet,
        options: CheckOptions<'_>,
    ) -> Result<CheckResult, AnalysisError> {
        let mut virtual_modules = interfaces.virtual_modules();
        virtual_modules.extend_from_slice(options.virtual_modules);
        self.check_with_options(
            source,
            CheckOptions {
                virtual_modules: &virtual_modules,
                ..options
            },
        )
        .await
    }

    /// Type-checks implementation source against a declaration interface in a set.
    ///
    /// The declaration stays registered under `declaration_specifier`. The implementation is
    /// added as an ad-hoc virtual module adjacent to `impl_module_id`, then a synthetic assignment
    /// checks that the implementation's exported value conforms to the declaration's `Module` type.
    /// Passing a source-path module id lets relative `require(...)` calls inside the
    /// implementation resolve exactly as they do when checking files directly. Passing a virtual
    /// module id lets relative calls resolve against sibling virtual modules with the same
    /// slash-delimited naming convention.
    pub async fn check_implementation(
        &mut self,
        impl_source: &str,
        impl_module_id: &ModuleId,
        interfaces: &ModuleInterfaceSet,
        declaration_specifier: &str,
    ) -> Result<CheckResult, AnalysisError> {
        let mut scoped = interfaces.clone();
        let implementation_specifier = implementation_check_specifier(impl_module_id.as_str());
        scoped.insert_implementation(&implementation_specifier, impl_source);
        let assertion = format!(
            "local _: typeof(require({declaration_specifier:?})) = require({implementation_specifier:?})"
        );
        let assertion_module_name = format!("{}$check", impl_module_id.as_str());
        self.check_with_interfaces_options(
            &assertion,
            &scoped,
            CheckOptions {
                module_name: Some(assertion_module_name.as_str()),
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

        let join = spawn_blocking(move || -> Result<CheckResult, AnalysisError> {
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
    ///
    /// The snapshot uses the static require policy documented in [`crate::resolver`]. Unsupported
    /// dynamic require expressions are rejected by analysis before a chunk is returned.
    pub async fn checked_load(
        &self,
        checker: &mut Checker,
        snapshot: ResolverSnapshot,
    ) -> Result<Chunk<'static>, AnalysisError> {
        self.checked_load_with_interfaces(checker, snapshot, &ModuleInterfaceSet::new())
            .await
    }

    /// Type-checks a resolver snapshot with host interfaces before returning a loadable chunk.
    ///
    /// Use `interfaces` for declaration-only host modules. If a runtime resolver returns
    /// interface-only source, snapshot construction fails before this method runs.
    pub async fn checked_load_with_interfaces(
        &self,
        checker: &mut Checker,
        snapshot: ResolverSnapshot,
        interfaces: &ModuleInterfaceSet,
    ) -> Result<Chunk<'static>, AnalysisError> {
        let result = checker
            .check_snapshot_with_interfaces(&snapshot, interfaces)
            .await?;
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
        let resolver: SharedResolver = Rc::new(snapshot);
        let cache: RuntimeModuleCache = Rc::new(RefCell::new(HashMap::new()));
        let env = resolver_environment(self, resolver, cache, Some(root_id.clone()))
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
        R: ModuleResolver + ?Sized,
    {
        let snapshot = ResolverSnapshot::resolve(resolver, root).await?;
        self.checked_load(checker, snapshot).await
    }
}

/// Returns a hidden checker-only module name that keeps the same parent path.
fn implementation_check_specifier(module_id: &str) -> String {
    format!("{module_id}$implementation")
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
/// Built on the runtime thread before `spawn_blocking` is invoked. All input
/// pointers passed to the C ABI are derived from boxed slices owned by this struct, so the
/// data outlives any caller borrows for the duration of the blocking work.
///
/// `virtual_module_entries` contains raw pointers into the boxed slices in retained
/// virtual module storage; both fields move together with the struct since the heap
/// allocations are pointed-at by stable addresses.
struct OwnedCheckInputs {
    module_id: ModuleId,
    source: Box<[u8]>,
    source_len: u32,
    module_name: Box<[u8]>,
    module_name_len: u32,
    timeout: Option<Duration>,
    /// Stable backing storage; pointers in `virtual_module_entries` reference these boxes.
    _virtual_module_storage: Vec<OwnedVirtualModule>,
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
            _virtual_module_storage: virtual_module_storage,
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
mod tests;
