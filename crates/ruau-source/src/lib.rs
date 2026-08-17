//! Async-first module source model shared by Ruau analysis and runtime.
//!
//! This crate defines canonical module identities, source reads, metadata, and
//! ready futures. It has no Tokio dependency. Use it directly for source-model
//! integrations, or through `ruau::source` from the umbrella crate.
//!
//! # Identities
//!
//! [`ModuleName`] is normalized UTF-8 text used by static analysis. It removes
//! portable path noise such as `./`, `..`, trailing slashes, and Luau source
//! extensions. [`ModuleId`] is the byte identity used by runtime `require`.
//! `ModuleId::new` keeps bytes verbatim for exact keys; use
//! [`ModuleId::canonicalized`] or [`ModuleId::from`] a `ModuleName` when the
//! value came from user input or a require-style path.
//!
//! # Sources
//!
//! [`InMemorySource`] is the test and embedding source for ready values.
//! [`RootSource`] provides a synthetic root buffer while delegating nested
//! `require` calls. [`Mounts`] composes
//! several sources behind prefixes such as `@user` and `@project`, with
//! mount-local epoch handles for precise invalidation.
//!
//! Custom [`SourceProvider`] implementations can use [`resolve_request_from`] for
//! the same relative require normalization as the built-in sources.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

mod combinators;
mod snapshot;

pub use combinators::{Router, SealedGraphSource};
pub use snapshot::{SnapshotSource, SourceGraphSnapshot, SourceResolutionEdge};

/// Canonical identity for one module.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModuleId(Vec<u8>);

impl ModuleId {
    /// Creates a module id from raw bytes, kept verbatim.
    ///
    /// No normalization happens: `"./foo"`, `"foo/"`, and `"foo"` produce
    /// three distinct ids. Use this for ids that are already resolved or
    /// canonical — values previously produced by [`ModuleId::canonicalized`]
    /// or [`ModuleName`], or synthesized exact keys (overlay roots,
    /// `@`-prefixed mount names) where the bytes ARE the identity.
    ///
    /// For user-supplied or require-style module paths, use
    /// [`ModuleId::canonicalized`] instead so lookups match the ids the
    /// resolver produces.
    #[must_use]
    pub fn new(id: impl Into<Vec<u8>>) -> Self {
        Self(id.into())
    }

    /// Creates a canonical module id from a portable path-like module name.
    ///
    /// The name is normalized via [`ModuleName::normalize`]: path segments
    /// are resolved (`.`/`..`), a leading `./` and any source extension are
    /// stripped, and trailing slashes are removed — so `"./foo.luau"` and
    /// `"foo"` produce the same id. Use this whenever the value flows from
    /// user input or require-style paths. Use [`ModuleId::new`] only for ids
    /// that are already exact keys.
    #[must_use]
    pub fn canonicalized(name: &str) -> Self {
        Self(ModuleName::normalize(name).into_inner().into_bytes())
    }

    /// Returns the canonical id bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the id as UTF-8 when possible.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    /// Returns a display-safe string for diagnostics.
    #[must_use]
    pub fn to_lossy_string(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }

    /// Returns a byte-exact string for diagnostics.
    ///
    /// Valid UTF-8 ids are returned as text. Invalid UTF-8 ids are rendered as
    /// escaped bytes so diagnostics do not collapse distinct canonical ids into
    /// the same replacement-character spelling.
    #[must_use]
    pub fn to_diagnostic_string(&self) -> String {
        let Some(text) = self.as_str() else {
            return escaped_bytes(&self.0);
        };
        text.to_owned()
    }
}

/// Normalized UTF-8 identity for one statically analyzed module.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModuleName(String);

impl ModuleName {
    /// Creates a normalized module name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self::normalize(name.into())
    }

    /// Returns the module name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the name and returns the normalized text.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Returns the parent module name, or an empty name at the root.
    #[must_use]
    pub fn parent(&self) -> Self {
        Self(
            self.0
                .rsplit_once("/")
                .map_or_else(String::new, |(parent, _)| parent.to_owned()),
        )
    }

    /// Converts a byte module id into a UTF-8 module name.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError`] when the id is not valid UTF-8.
    pub fn from_id(id: &ModuleId) -> SourceResult<Self> {
        id.as_str()
            .map(|name| Self(name.to_owned()))
            .ok_or_else(|| SourceError::other(format!("module id '{}' is not UTF-8", id)))
    }

    /// Normalizes a portable module name.
    #[must_use]
    pub fn normalize(name: impl AsRef<str>) -> Self {
        let name = name.as_ref();
        let normalized = normalize_path(name);
        let trimmed = normalized.strip_prefix("./").unwrap_or(&normalized);
        let trimmed = strip_source_extension(trimmed).unwrap_or(trimmed);
        Self(trimmed.trim_end_matches('/').to_owned())
    }

    /// Joins portable module-name fragments into a normalized name.
    #[must_use]
    pub fn join(lhs: &str, rhs: &str) -> Self {
        if lhs.is_empty() {
            Self::normalize(rhs)
        } else {
            Self::normalize(format!("{lhs}/{rhs}"))
        }
    }
}

impl AsRef<[u8]> for ModuleId {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_diagnostic_string())
    }
}

impl From<&ModuleName> for ModuleId {
    fn from(value: &ModuleName) -> Self {
        Self::new(value.as_str().as_bytes().to_vec())
    }
}

impl fmt::Display for ModuleName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for ModuleName {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ModuleName {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&[u8]> for ModuleId {
    fn from(value: &[u8]) -> Self {
        Self::new(value.to_vec())
    }
}

impl From<Vec<u8>> for ModuleId {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

fn escaped_bytes(bytes: &[u8]) -> String {
    use fmt::Write as _;

    let mut escaped = String::new();
    for &byte in bytes {
        match byte {
            b'\\' => escaped.push_str("\\\\"),
            b'\n' => escaped.push_str("\\n"),
            b'\r' => escaped.push_str("\\r"),
            b'\t' => escaped.push_str("\\t"),
            0x20..=0x7e => escaped.push(byte as char),
            _ => write!(&mut escaped, "\\x{byte:02X}").expect("writing to a string cannot fail"),
        }
    }
    escaped
}

/// Display metadata attached to one source module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceMetadata {
    /// Human-readable name used in diagnostics.
    pub display_name: String,
    /// Environment/profile name, when the source assigns one.
    pub environment: Option<String>,
}

impl SourceMetadata {
    /// Creates metadata with a display name and no environment.
    #[must_use]
    pub fn new(display_name: impl Into<String>) -> Self {
        Self {
            display_name: display_name.into(),
            environment: None,
        }
    }

    /// Creates metadata with an environment name.
    #[must_use]
    pub fn with_environment(
        display_name: impl Into<String>,
        environment: impl Into<String>,
    ) -> Self {
        Self {
            display_name: display_name.into(),
            environment: Some(environment.into()),
        }
    }
}

/// One source buffer plus the identity used for diagnostics and VM loading.
///
/// The [`ModuleId`] is the byte-exact identity for the source. Its
/// [`SourceMetadata`] supplies the human-readable diagnostic and traceback name,
/// while the module id remains the requester identity used for resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Source {
    id: ModuleId,
    source: Vec<u8>,
    metadata: SourceMetadata,
}

impl Source {
    /// Creates a source from UTF-8 text.
    #[must_use]
    pub fn text(id: impl Into<ModuleId>, source: impl Into<String>) -> Self {
        Self::bytes(id, source.into().into_bytes())
    }

    /// Creates a source from byte-exact source.
    #[must_use]
    pub fn bytes(id: impl Into<ModuleId>, source: impl Into<Vec<u8>>) -> Self {
        let id = id.into();
        let metadata = SourceMetadata::new(id.to_diagnostic_string());
        Self {
            id,
            source: source.into(),
            metadata,
        }
    }

    /// Replaces the diagnostic metadata for this source.
    #[must_use]
    pub fn with_metadata(mut self, metadata: SourceMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Returns the byte-exact identity for this source.
    #[must_use]
    pub const fn id(&self) -> &ModuleId {
        &self.id
    }

    /// Returns the source bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.source
    }

    /// Consumes this source and returns its byte buffer.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.source
    }

    /// Returns the source as UTF-8 text when possible.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.source).ok()
    }

    /// Returns diagnostic display metadata for this source.
    #[must_use]
    pub const fn metadata(&self) -> &SourceMetadata {
        &self.metadata
    }

    /// Returns the human-readable diagnostic display name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.metadata.display_name
    }

    /// Returns the human-facing Lua chunk name bytes for `Vm::load_named`.
    ///
    /// This derives from display metadata; [`Self::id`] remains the separate runtime requester
    /// identity passed to named-module loading.
    #[must_use]
    pub fn load_name(&self) -> Vec<u8> {
        chunk_load_name(self.metadata.display_name.as_bytes())
    }
}

/// Returns Lua chunk-name bytes for a source identity.
///
/// Names already marked with `=` or `@` are preserved. Other identities are
/// treated as file-like names and prefixed with `@`.
#[must_use]
fn chunk_load_name(name: impl AsRef<[u8]>) -> Vec<u8> {
    let name = name.as_ref();
    if matches!(name.first(), Some(b'=' | b'@')) {
        name.to_vec()
    } else {
        let mut load_name = Vec::with_capacity(name.len() + 1);
        load_name.push(b'@');
        load_name.extend_from_slice(name);
        load_name
    }
}

/// Result type for async source operations.
pub type SourceResult<T> = Result<T, SourceError>;

/// Boxed future returned by [`SourceProvider`] operations.
#[cfg(not(target_arch = "wasm32"))]
pub type SourceFuture<T> = Pin<Box<dyn Future<Output = SourceResult<T>> + Send>>;
/// The boxed future a module source returns (wasm: no `Send` bound; the
/// executor is single-threaded and JS-backed futures are `!Send`).
#[cfg(target_arch = "wasm32")]
pub type SourceFuture<T> = Pin<Box<dyn Future<Output = SourceResult<T>>>>;

/// Request to read a resolved module source.
///
/// `id` is the canonical id produced by [`SourceProvider::resolve`]. `requester`
/// is the canonical module id of the module that issued the request, when the
/// read happens for a nested `require`. Most sources ignore the requester and
/// read by id only; sources that synthesize requester-specific wrapper modules
/// can use it to choose source bytes without encoding requester text into `id`.
#[derive(Clone, Copy, Debug)]
pub struct ReadContext<'a> {
    id: &'a ModuleId,
    requester: Option<&'a ModuleId>,
}

/// One atomic observation of a resolved module source.
///
/// The source bytes, VM instance identity, provider epoch, and optional physical
/// origin all describe the same read. The origin is a lossless host path rather
/// than display metadata; providers without a physical backing file leave it
/// absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRead {
    source: Source,
    instance: InstanceKey,
    epoch: u64,
    origin: Option<PathBuf>,
}

impl SourceRead {
    /// Creates one complete source-read observation.
    #[must_use]
    pub fn new(source: Source, instance: InstanceKey, epoch: u64, origin: Option<PathBuf>) -> Self {
        Self {
            source,
            instance,
            epoch,
            origin,
        }
    }

    /// Returns the observed source.
    #[must_use]
    pub const fn source(&self) -> &Source {
        &self.source
    }

    /// Consumes the observation and returns all observed fields.
    #[must_use]
    pub fn into_parts(self) -> (Source, InstanceKey, u64, Option<PathBuf>) {
        (self.source, self.instance, self.epoch, self.origin)
    }

    /// Returns the VM export-cache identity observed for the source.
    #[must_use]
    pub const fn instance_key(&self) -> &InstanceKey {
        &self.instance
    }

    /// Returns the source-provider epoch observed with the source.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the lossless physical origin, when the provider has one.
    #[must_use]
    pub fn origin(&self) -> Option<&Path> {
        self.origin.as_deref()
    }
}

impl<'a> ReadContext<'a> {
    /// Creates a read request for `id` with no requester.
    #[must_use]
    pub const fn new(id: &'a ModuleId) -> Self {
        Self {
            id,
            requester: None,
        }
    }

    /// Creates a read request for `id` and an optional requester.
    #[must_use]
    pub const fn with_requester(id: &'a ModuleId, requester: Option<&'a ModuleId>) -> Self {
        Self { id, requester }
    }

    /// Returns the resolved module id being read.
    #[must_use]
    pub const fn id(&self) -> &'a ModuleId {
        self.id
    }

    /// Returns the module that requested this read, if known.
    #[must_use]
    pub const fn requester(&self) -> Option<&'a ModuleId> {
        self.requester
    }
}

/// VM cache identity for one resolved module instance.
///
/// The default key is shared by resolved id. Sources that generate distinct
/// source bodies for the same id can return a requester-scoped key from
/// [`SourceProvider::instance_key`].
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstanceKey {
    id: ModuleId,
    requester: Option<ModuleId>,
}

impl InstanceKey {
    /// Creates a cache key from its parts.
    #[must_use]
    pub fn new(id: ModuleId, requester: Option<ModuleId>) -> Self {
        Self { id, requester }
    }

    /// Creates the default cache key shared by every requester of `id`.
    #[must_use]
    pub fn shared(id: ModuleId) -> Self {
        Self::new(id, None)
    }

    /// Creates a cache key scoped to `requester`.
    #[must_use]
    pub fn per_requester(id: ModuleId, requester: ModuleId) -> Self {
        Self::new(id, Some(requester))
    }

    /// Returns the resolved module id.
    #[must_use]
    pub const fn id(&self) -> &ModuleId {
        &self.id
    }

    /// Returns the requester component, if this key is requester-scoped.
    #[must_use]
    pub const fn requester(&self) -> Option<&ModuleId> {
        self.requester.as_ref()
    }
}

/// Failure raised by module source operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceError {
    /// The requested module id has no source.
    MissingModule {
        /// Canonical id that was not found.
        id: ModuleId,
    },
    /// A relative request had no requester context.
    UnresolvableRelativeRequest {
        /// Request bytes that could not be anchored.
        request: Vec<u8>,
    },
    /// A source future returned [`Poll::Pending`] in a ready-only caller.
    Pending {
        /// Operation that was being polled.
        operation: &'static str,
    },
    /// The source provider changed while a graph snapshot was being captured.
    EpochChanged {
        /// Epoch pinned by the snapshot.
        expected: u64,
        /// Epoch observed after the operation.
        observed: u64,
    },
    /// A sealed graph source rejected an operation outside its captured closure.
    UncapturedOperation {
        /// Human-readable operation detail.
        operation: String,
    },
    /// One module id resolved to incompatible requester-specific instances.
    AmbiguousInstance {
        /// Module id whose graph representation would be ambiguous.
        id: ModuleId,
    },
    /// The source implementation rejected a request.
    Other {
        /// Human-readable detail.
        message: String,
    },
}

impl SourceError {
    /// Creates an implementation-defined error.
    #[must_use]
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other {
            message: message.into(),
        }
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingModule { id } => write!(formatter, "module '{id}' not found"),
            Self::UnresolvableRelativeRequest { request } => write!(
                formatter,
                "relative module request '{}' has no requester context",
                String::from_utf8_lossy(request)
            ),
            Self::Pending { operation } => {
                write!(
                    formatter,
                    "async entry required: module source was pending while {operation}"
                )
            }
            Self::EpochChanged { expected, observed } => write!(
                formatter,
                "source epoch changed while capturing a graph (expected {expected}, observed {observed})"
            ),
            Self::UncapturedOperation { operation } => {
                write!(formatter, "sealed source rejected uncaptured {operation}")
            }
            Self::AmbiguousInstance { id } => {
                write!(
                    formatter,
                    "module '{id}' has incompatible requester-specific source instances"
                )
            }
            Self::Other { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SourceError {}

/// Async-first source of module ids, bytes, and metadata.
pub trait SourceProvider: Send + Sync {
    /// Resolves `request` from `requester` to a canonical id.
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId>;

    /// Reads source bytes for `id`.
    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>>;

    /// Reads source bytes for a resolved request.
    ///
    /// The default implementation ignores requester context and calls
    /// [`Self::read`], preserving the ordinary id-only source contract.
    fn read_request(&self, request: ReadContext<'_>) -> SourceFuture<Vec<u8>> {
        self.read(request.id())
    }

    /// Atomically observes source bytes, identity, epoch, and physical origin.
    ///
    /// This default is correct for immutable providers. Providers with mutable
    /// internal state or physical backing storage must override it so every
    /// field comes from one coherent observation.
    fn read_observation(&self, request: ReadContext<'_>) -> SourceFuture<SourceRead> {
        let id = request.id().clone();
        let metadata = self.metadata(&id);
        let instance = self.instance_key(request);
        let epoch = self.epoch();
        let future = self.read_request(request);
        Box::pin(async move {
            let source = Source::bytes(id, future.await?).with_metadata(metadata);
            Ok(SourceRead::new(source, instance, epoch, None))
        })
    }

    /// Returns the VM export-cache key for a resolved request.
    ///
    /// The default key is shared by resolved id. Sources that make
    /// requester-specific source bodies should override this alongside
    /// [`Self::read_request`] so two requesters do not share exports.
    fn instance_key(&self, request: ReadContext<'_>) -> InstanceKey {
        InstanceKey::shared(request.id().clone())
    }

    /// Returns display metadata for `id`.
    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        SourceMetadata::new(id.to_lossy_string())
    }

    /// Returns an invalidation epoch for this source.
    ///
    /// The VM export cache stores completed modules under their canonical id and
    /// records the epoch, so changing the epoch invalidates previously cached
    /// exports in that VM without growing cache keys forever.
    fn epoch(&self) -> u64 {
        0
    }
}

/// Synchronous module source model.
///
/// Implement this for sources that can answer immediately. The blanket
/// [`SourceProvider`] impl wraps each result in an immediately-ready future, so
/// callers can install a synchronous source without hand-writing `ready(...)`
/// boilerplate.
pub trait SyncSourceProvider: Send + Sync {
    /// Synchronously resolves `request` from `requester` to a canonical id.
    fn resolve_sync(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceResult<ModuleId>;

    /// Synchronously reads source bytes for `id`.
    fn read_sync(&self, id: &ModuleId) -> SourceResult<Vec<u8>>;

    /// Synchronously reads source bytes for a resolved request.
    fn read_request_sync(&self, request: ReadContext<'_>) -> SourceResult<Vec<u8>> {
        self.read_sync(request.id())
    }

    /// Atomically observes source bytes, identity, epoch, and physical origin.
    ///
    /// This default is correct for immutable providers. Stateful providers must
    /// override it so every field comes from one coherent observation.
    fn read_observation_sync(&self, request: ReadContext<'_>) -> SourceResult<SourceRead> {
        let id = request.id().clone();
        let source = Source::bytes(id.clone(), self.read_request_sync(request)?)
            .with_metadata(self.metadata(&id));
        Ok(SourceRead::new(
            source,
            self.instance_key(request),
            self.epoch(),
            None,
        ))
    }

    /// Returns display metadata for `id`.
    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        SourceMetadata::new(id.to_lossy_string())
    }

    /// Returns an invalidation epoch for this source.
    fn epoch(&self) -> u64 {
        0
    }

    /// Returns the VM export-cache key for a resolved request.
    fn instance_key(&self, request: ReadContext<'_>) -> InstanceKey {
        InstanceKey::shared(request.id().clone())
    }
}

impl<T: SyncSourceProvider> SourceProvider for T {
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId> {
        ready(self.resolve_sync(requester, request))
    }

    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
        ready(self.read_sync(id))
    }

    fn read_request(&self, request: ReadContext<'_>) -> SourceFuture<Vec<u8>> {
        ready(self.read_request_sync(request))
    }

    fn read_observation(&self, request: ReadContext<'_>) -> SourceFuture<SourceRead> {
        ready(self.read_observation_sync(request))
    }

    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        SyncSourceProvider::metadata(self, id)
    }

    fn epoch(&self) -> u64 {
        SyncSourceProvider::epoch(self)
    }

    fn instance_key(&self, request: ReadContext<'_>) -> InstanceKey {
        SyncSourceProvider::instance_key(self, request)
    }
}

/// Creates an immediately-ready module source future.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn ready<T: Send + 'static>(result: SourceResult<T>) -> SourceFuture<T> {
    Box::pin(std::future::ready(result))
}

/// Creates an immediately-ready module source future (wasm: no `Send` bound,
/// matching [`SourceFuture`]).
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn ready<T: 'static>(result: SourceResult<T>) -> SourceFuture<T> {
    Box::pin(std::future::ready(result))
}

/// Ready-only bridge for native callers that cannot drive an async provider.
pub trait ReadySourceFutureExt<T> {
    /// Polls this source operation once and reports a pending provider explicitly.
    ///
    /// A truly async source must be driven by an async entry point instead.
    fn ready_only(self, operation: &'static str) -> SourceResult<T>;
}

impl<T> ReadySourceFutureExt<T> for SourceFuture<T> {
    fn ready_only(mut self, operation: &'static str) -> SourceResult<T> {
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        match self.as_mut().poll(&mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(SourceError::Pending { operation }),
        }
    }
}

/// In-memory [`SourceProvider`] implementation.
#[derive(Clone, Debug, Default)]
pub struct InMemorySource {
    modules: HashMap<ModuleId, Vec<u8>>,
    aliases: HashMap<ModuleId, ModuleId>,
    metadata: HashMap<ModuleId, SourceMetadata>,
    epoch: u64,
}

impl InMemorySource {
    /// Creates an empty in-memory source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `source` under `id`, replacing any previous source. Chainable.
    #[must_use]
    pub fn with_module(mut self, id: impl Into<ModuleId>, source: impl AsRef<[u8]>) -> Self {
        self.insert(id, source);
        self
    }

    /// Registers `source` with its embedded metadata. Chainable.
    #[must_use]
    pub fn with_source(mut self, source: &Source) -> Self {
        self.insert_source(source);
        self
    }

    /// Registers `source` under `id` with display metadata. Chainable.
    #[must_use]
    pub fn with_module_metadata(
        mut self,
        id: impl Into<ModuleId>,
        source: impl AsRef<[u8]>,
        metadata: SourceMetadata,
    ) -> Self {
        self.insert_with_metadata(id, source, metadata);
        self
    }

    /// Registers `source` under a normalized module name. Chainable.
    #[must_use]
    pub fn with_module_name(
        mut self,
        name: impl Into<ModuleName>,
        source: impl AsRef<[u8]>,
    ) -> Self {
        self.insert_module_name(name, source);
        self
    }

    /// Registers display metadata under `id`. Chainable.
    #[must_use]
    pub fn with_metadata(mut self, id: impl Into<ModuleId>, metadata: SourceMetadata) -> Self {
        self.set_metadata(id, metadata);
        self
    }

    /// Registers `alias` as another spelling for `target`. Chainable.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<ModuleId>, target: impl Into<ModuleId>) -> Self {
        self.set_alias(alias, target);
        self
    }

    /// Registers `source` under `id`, replacing any previous source.
    pub fn insert(&mut self, id: impl Into<ModuleId>, source: impl AsRef<[u8]>) {
        self.modules.insert(id.into(), source.as_ref().to_vec());
        self.bump_epoch();
    }

    /// Registers a complete [`Source`], including display metadata.
    pub fn insert_source(&mut self, source: &Source) {
        let id = source.id().clone();
        self.modules.insert(id.clone(), source.as_bytes().to_vec());
        self.metadata.insert(id, source.metadata().clone());
        self.bump_epoch();
    }

    /// Registers `source` and display metadata under `id`.
    pub fn insert_with_metadata(
        &mut self,
        id: impl Into<ModuleId>,
        source: impl AsRef<[u8]>,
        metadata: SourceMetadata,
    ) {
        let id = id.into();
        self.modules.insert(id.clone(), source.as_ref().to_vec());
        self.metadata.insert(id, metadata);
        self.bump_epoch();
    }

    /// Registers `source` under a normalized module name.
    pub fn insert_module_name(&mut self, name: impl Into<ModuleName>, source: impl AsRef<[u8]>) {
        let name = name.into();
        self.insert(ModuleId::from(&name), source);
    }

    /// Registers display metadata under `id`.
    pub fn set_metadata(&mut self, id: impl Into<ModuleId>, metadata: SourceMetadata) {
        self.metadata.insert(id.into(), metadata);
        self.bump_epoch();
    }

    /// Registers `alias` as another spelling for `target`.
    pub fn set_alias(&mut self, alias: impl Into<ModuleId>, target: impl Into<ModuleId>) {
        self.aliases.insert(alias.into(), target.into());
        self.bump_epoch();
    }

    /// Removes an alias, returning whether one existed.
    pub fn remove_alias(&mut self, alias: impl Into<ModuleId>) -> bool {
        let removed = self.aliases.remove(&alias.into()).is_some();
        if removed {
            self.bump_epoch();
        }
        removed
    }

    fn bump_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    fn resolve_alias(&self, id: &ModuleId) -> SourceResult<ModuleId> {
        let mut current = id.clone();
        let mut seen = HashSet::new();
        while let Some(next) = self.aliases.get(&current) {
            if !seen.insert(current.clone()) {
                return Err(SourceError::other(format!(
                    "module alias cycle involving '{}'",
                    current
                )));
            }
            current = next.clone();
        }
        Ok(current)
    }
}

impl SyncSourceProvider for InMemorySource {
    fn resolve_sync(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceResult<ModuleId> {
        let id = resolve_request(requester, request)?;
        self.resolve_alias(&id)
    }

    fn read_sync(&self, id: &ModuleId) -> SourceResult<Vec<u8>> {
        let id = self.resolve_alias(id)?;
        self.modules
            .get(&id)
            .cloned()
            .ok_or(SourceError::MissingModule { id })
    }

    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        let id = self.resolve_alias(id).unwrap_or_else(|_| id.clone());
        self.metadata
            .get(&id)
            .cloned()
            .unwrap_or_else(|| SourceMetadata::new(id.to_lossy_string()))
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// Source adapter that overlays one synthetic root module over an optional
/// delegate source graph.
pub struct RootSource {
    root_id: ModuleId,
    root_name: ModuleName,
    root_display_name: String,
    root_source: Vec<u8>,
    delegate: Option<Arc<dyn SourceProvider>>,
    root_requester: Option<ModuleId>,
}

impl RootSource {
    /// Creates a root overlay with no delegate.
    #[must_use]
    pub fn new(root_id: impl Into<ModuleId>, root_source: impl Into<Vec<u8>>) -> Self {
        let root_id = root_id.into();
        let root_name = ModuleName::from_id(&root_id)
            .unwrap_or_else(|_| ModuleName::from(root_id.to_lossy_string()));
        Self {
            root_display_name: root_id.to_lossy_string(),
            root_id,
            root_name,
            root_source: root_source.into(),
            delegate: None,
            root_requester: None,
        }
    }

    /// Returns the synthetic root id.
    #[must_use]
    pub const fn root_id(&self) -> &ModuleId {
        &self.root_id
    }

    /// Returns the root name used by source-graph checkers.
    #[must_use]
    pub fn root_name(&self) -> ModuleName {
        self.root_name.clone()
    }

    /// Sets the root name used by source-graph checkers.
    #[must_use]
    pub fn with_root_name(mut self, root_name: impl Into<ModuleName>) -> Self {
        self.root_name = root_name.into();
        self
    }

    /// Sets the display name reported for the synthetic root.
    #[must_use]
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.root_display_name = display_name.into();
        self
    }

    /// Delegates non-root reads and nested resolution to an owned source.
    #[must_use]
    pub fn with_delegate(mut self, delegate: Arc<dyn SourceProvider>) -> Self {
        self.delegate = Some(delegate);
        self
    }

    /// Resolves root-relative requests as though they were issued by
    /// `requester`.
    #[must_use]
    pub fn with_root_requester(mut self, requester: impl Into<ModuleId>) -> Self {
        self.root_requester = Some(requester.into());
        self
    }

    fn resolve_without_delegate(
        &self,
        requester: Option<&ModuleId>,
        request: &[u8],
    ) -> SourceFuture<ModuleId> {
        ready(resolve_request(requester, request))
    }
}

impl SourceProvider for RootSource {
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId> {
        let delegate_requester = if requester == Some(&self.root_id) {
            self.root_requester.as_ref()
        } else {
            requester
        };
        if resolve_request(delegate_requester, request).is_ok_and(|id| id == self.root_id) {
            return ready(Ok(self.root_id.clone()));
        }
        let future = self.delegate.as_ref().map_or_else(
            || self.resolve_without_delegate(delegate_requester, request),
            |source| source.resolve(delegate_requester, request),
        );
        let root_id = self.root_id.clone();
        Box::pin(async move {
            let id = future.await?;
            if id == root_id {
                return Err(SourceError::other(format!(
                    "module id '{root_id}' is reserved for the root overlay"
                )));
            }
            Ok(id)
        })
    }

    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
        if id == &self.root_id {
            return ready(Ok(self.root_source.clone()));
        }
        self.delegate.as_ref().map_or_else(
            || ready(Err(SourceError::MissingModule { id: id.clone() })),
            |source| source.read(id),
        )
    }

    fn read_observation(&self, request: ReadContext<'_>) -> SourceFuture<SourceRead> {
        if request.id() == &self.root_id {
            let source = Source::bytes(self.root_id.clone(), self.root_source.clone())
                .with_metadata(SourceMetadata::new(self.root_display_name.clone()));
            return ready(Ok(SourceRead::new(
                source,
                InstanceKey::shared(self.root_id.clone()),
                self.epoch(),
                None,
            )));
        }
        self.delegate.as_ref().map_or_else(
            || {
                ready(Err(SourceError::MissingModule {
                    id: request.id().clone(),
                }))
            },
            |source| source.read_observation(request),
        )
    }

    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        if id == &self.root_id {
            return SourceMetadata::new(self.root_display_name.clone());
        }
        self.delegate.as_ref().map_or_else(
            || SourceMetadata::new(id.to_lossy_string()),
            |source| source.metadata(id),
        )
    }

    fn epoch(&self) -> u64 {
        self.delegate.as_ref().map_or(0, |source| source.epoch())
    }
}

/// Shared cache-invalidation epoch for source providers and mounts.
///
/// Clone and keep this handle when a mounted source has host-side invalidation
/// state that is not represented by the child source's own [`SourceProvider::epoch`].
/// Bumping it changes the parent [`Mounts::epoch`] without rebuilding the
/// mount table.
#[derive(Clone, Debug)]
pub struct SourceEpoch {
    value: Arc<AtomicU64>,
}

impl SourceEpoch {
    /// Creates an epoch handle initialized to `value`.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self {
            value: Arc::new(AtomicU64::new(value)),
        }
    }

    /// Returns the current epoch value.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::SeqCst)
    }

    /// Sets the epoch to `value`.
    pub fn set(&self, value: u64) {
        self.value.store(value, Ordering::SeqCst);
    }

    /// Increments the epoch, wrapping on overflow, and returns the new value.
    pub fn bump(&self) -> u64 {
        self.value.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }
}

impl Default for SourceEpoch {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Composite [`SourceProvider`] that dispatches requests by mounted prefix.
///
/// A mounted source keeps each child source's internal module ids private. Public
/// resolved ids are prefixed with the mount name, so runtime export caches cannot
/// collide when two mounts resolve the same child id.
#[derive(Clone, Default)]
pub struct Mounts {
    mounts: Vec<ModuleMount>,
}

#[derive(Clone)]
struct ModuleMount {
    prefix: ModuleId,
    source: Arc<dyn SourceProvider>,
    epoch: SourceEpoch,
}

/// Builder for a mounted source provider.
#[derive(Clone, Default)]
pub struct MountsBuilder {
    mounts: Mounts,
}

impl MountsBuilder {
    /// Adds a source under `prefix` with a fresh epoch handle.
    #[must_use]
    pub fn with_mount(
        mut self,
        prefix: impl Into<ModuleId>,
        source: Arc<dyn SourceProvider>,
    ) -> Self {
        self.mounts.insert(prefix, source, SourceEpoch::default());
        self
    }

    /// Adds a source under `prefix` with a shared epoch handle.
    #[must_use]
    pub fn with_mount_epoch(
        mut self,
        prefix: impl Into<ModuleId>,
        source: Arc<dyn SourceProvider>,
        epoch: SourceEpoch,
    ) -> Self {
        self.mounts.insert(prefix, source, epoch);
        self
    }

    /// Builds the mounted source provider.
    #[must_use]
    pub fn build(self) -> Mounts {
        self.mounts
    }
}

impl Mounts {
    /// Creates a mounted source builder.
    #[must_use]
    pub fn builder() -> MountsBuilder {
        MountsBuilder::default()
    }

    /// Inserts `source` under `prefix` with its invalidation epoch.
    ///
    /// Earlier mounts win when prefixes overlap.
    pub fn insert(
        &mut self,
        prefix: impl Into<ModuleId>,
        source: Arc<dyn SourceProvider>,
        epoch: SourceEpoch,
    ) {
        self.mounts.push(ModuleMount {
            prefix: normalize_mount_prefix(&prefix.into()),
            source,
            epoch,
        });
    }

    fn mount_for_request(&self, request: &str) -> Option<&ModuleMount> {
        self.mounts.iter().find(|mount| {
            mount
                .prefix
                .as_str()
                .is_some_and(|prefix| strip_prefix_text(request, prefix).is_some())
        })
    }

    fn mount_for_id(&self, id: &ModuleId) -> Option<&ModuleMount> {
        self.mounts
            .iter()
            .find(|mount| strip_prefix_bytes(id.as_bytes(), mount.prefix.as_bytes()).is_some())
    }
}

impl fmt::Debug for Mounts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Mounts")
            .field(
                "mounts",
                &self
                    .mounts
                    .iter()
                    .map(|mount| mount.prefix.to_lossy_string())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl SourceProvider for Mounts {
    fn resolve(&self, requester: Option<&ModuleId>, request: &[u8]) -> SourceFuture<ModuleId> {
        let Ok(request_text) = std::str::from_utf8(request) else {
            return ready(Err(SourceError::other(
                "mounted module request is not UTF-8",
            )));
        };
        if is_relative_request(request_text) {
            let Some(requester) = requester else {
                return ready(Err(SourceError::UnresolvableRelativeRequest {
                    request: request.to_vec(),
                }));
            };
            let Some(mount) = self.mount_for_id(requester) else {
                return ready(Err(SourceError::MissingModule {
                    id: requester.clone(),
                }));
            };
            let Some(inner_requester) =
                strip_prefix_bytes(requester.as_bytes(), mount.prefix.as_bytes())
            else {
                return ready(Err(SourceError::MissingModule {
                    id: requester.clone(),
                }));
            };
            let source = Arc::clone(&mount.source);
            let prefix = mount.prefix.clone();
            let inner_requester = ModuleId::new(inner_requester.to_vec());
            let request = request.to_vec();
            return Box::pin(async move {
                source
                    .resolve(Some(&inner_requester), &request)
                    .await
                    .map(|id| prefix_id(&prefix, &id))
            });
        }

        let Some(mount) = self.mount_for_request(request_text) else {
            return ready(Err(SourceError::MissingModule {
                id: ModuleId::canonicalized(request_text),
            }));
        };
        let inner = strip_prefix_text(request_text, mount.prefix.as_str().unwrap_or_default())
            .expect("mount matched the request");
        let source = Arc::clone(&mount.source);
        let prefix = mount.prefix.clone();
        let inner = inner.as_bytes().to_vec();
        Box::pin(async move {
            source
                .resolve(None, &inner)
                .await
                .map(|id| prefix_id(&prefix, &id))
        })
    }

    fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
        let Some(mount) = self.mount_for_id(id) else {
            return ready(Err(SourceError::MissingModule { id: id.clone() }));
        };
        let Some(inner) = strip_prefix_bytes(id.as_bytes(), mount.prefix.as_bytes()) else {
            return ready(Err(SourceError::MissingModule { id: id.clone() }));
        };
        let source = Arc::clone(&mount.source);
        let inner = ModuleId::new(inner.to_vec());
        Box::pin(async move { source.read(&inner).await })
    }

    fn read_request(&self, request: ReadContext<'_>) -> SourceFuture<Vec<u8>> {
        let Some(mount) = self.mount_for_id(request.id()) else {
            return ready(Err(SourceError::MissingModule {
                id: request.id().clone(),
            }));
        };
        let Some(inner_id) = strip_prefix_bytes(request.id().as_bytes(), mount.prefix.as_bytes())
        else {
            return ready(Err(SourceError::MissingModule {
                id: request.id().clone(),
            }));
        };
        let inner_requester = request.requester().and_then(|requester| {
            strip_prefix_bytes(requester.as_bytes(), mount.prefix.as_bytes())
                .map(|inner| ModuleId::new(inner.to_vec()))
        });
        let source = Arc::clone(&mount.source);
        let inner_id = ModuleId::new(inner_id.to_vec());
        Box::pin(async move {
            source
                .read_request(ReadContext::with_requester(
                    &inner_id,
                    inner_requester.as_ref(),
                ))
                .await
        })
    }

    fn read_observation(&self, request: ReadContext<'_>) -> SourceFuture<SourceRead> {
        let Some(mount) = self.mount_for_id(request.id()) else {
            return ready(Err(SourceError::MissingModule {
                id: request.id().clone(),
            }));
        };
        let Some(inner_id) = strip_prefix_bytes(request.id().as_bytes(), mount.prefix.as_bytes())
        else {
            return ready(Err(SourceError::MissingModule {
                id: request.id().clone(),
            }));
        };
        let inner_requester = request.requester().and_then(|requester| {
            strip_prefix_bytes(requester.as_bytes(), mount.prefix.as_bytes())
                .map(|inner| ModuleId::new(inner.to_vec()))
        });
        let source = Arc::clone(&mount.source);
        let prefix = mount.prefix.clone();
        let inner_id = ModuleId::new(inner_id.to_vec());
        let mounted = self.clone();
        let epoch = self.epoch();
        Box::pin(async move {
            let read = source
                .read_observation(ReadContext::with_requester(
                    &inner_id,
                    inner_requester.as_ref(),
                ))
                .await?;
            let observed = mounted.epoch();
            if observed != epoch {
                return Err(SourceError::EpochChanged {
                    expected: epoch,
                    observed,
                });
            }
            let inner_source = read.source();
            let mut metadata = inner_source.metadata().clone();
            metadata.display_name =
                format!("{}/{}", prefix.to_lossy_string(), metadata.display_name);
            let mounted_source = Source::bytes(
                prefix_id(&prefix, inner_source.id()),
                inner_source.as_bytes().to_vec(),
            )
            .with_metadata(metadata);
            let instance = InstanceKey::new(
                prefix_id(&prefix, read.instance_key().id()),
                read.instance_key()
                    .requester()
                    .map(|requester| prefix_id(&prefix, requester)),
            );
            Ok(SourceRead::new(
                mounted_source,
                instance,
                epoch,
                read.origin().map(Path::to_path_buf),
            ))
        })
    }

    fn instance_key(&self, request: ReadContext<'_>) -> InstanceKey {
        let Some(mount) = self.mount_for_id(request.id()) else {
            return InstanceKey::shared(request.id().clone());
        };
        let Some(inner_id) = strip_prefix_bytes(request.id().as_bytes(), mount.prefix.as_bytes())
        else {
            return InstanceKey::shared(request.id().clone());
        };
        let inner_requester = request.requester().and_then(|requester| {
            strip_prefix_bytes(requester.as_bytes(), mount.prefix.as_bytes())
                .map(|inner| ModuleId::new(inner.to_vec()))
        });
        let inner_id = ModuleId::new(inner_id.to_vec());
        let key = mount.source.instance_key(ReadContext::with_requester(
            &inner_id,
            inner_requester.as_ref(),
        ));
        InstanceKey::new(
            prefix_id(&mount.prefix, key.id()),
            key.requester()
                .map(|requester| prefix_id(&mount.prefix, requester)),
        )
    }

    fn metadata(&self, id: &ModuleId) -> SourceMetadata {
        let Some(mount) = self.mount_for_id(id) else {
            return SourceMetadata::new(id.to_lossy_string());
        };
        let Some(inner) = strip_prefix_bytes(id.as_bytes(), mount.prefix.as_bytes()) else {
            return SourceMetadata::new(id.to_lossy_string());
        };
        let inner = ModuleId::new(inner.to_vec());
        let mut metadata = mount.source.metadata(&inner);
        metadata.display_name = format!(
            "{}/{}",
            mount.prefix.to_lossy_string(),
            metadata.display_name
        );
        metadata
    }

    fn epoch(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for mount in &self.mounts {
            for byte in mount.prefix.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            for byte in mount.source.epoch().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            for byte in mount.epoch.get().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }
}

fn normalize_mount_prefix(prefix: &ModuleId) -> ModuleId {
    prefix
        .as_str()
        .map_or(prefix.clone(), ModuleId::canonicalized)
}

fn strip_prefix_text<'a>(request: &'a str, prefix: &str) -> Option<&'a str> {
    if request == prefix {
        return Some("");
    }
    request.strip_prefix(prefix)?.strip_prefix('/')
}

fn strip_prefix_bytes<'a>(id: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if id == prefix {
        return Some(&[]);
    }
    id.strip_prefix(prefix)?.strip_prefix(b"/")
}

fn prefix_id(prefix: &ModuleId, id: &ModuleId) -> ModuleId {
    if id.as_bytes().is_empty() {
        return prefix.clone();
    }
    let mut prefixed = Vec::with_capacity(prefix.as_bytes().len() + 1 + id.as_bytes().len());
    prefixed.extend_from_slice(prefix.as_bytes());
    prefixed.push(b'/');
    prefixed.extend_from_slice(id.as_bytes());
    ModuleId::new(prefixed)
}

/// Resolves a concrete request string to a canonical module id.
///
/// Non-UTF-8 requests are treated as already-canonical opaque ids.
pub fn resolve_request(requester: Option<&ModuleId>, request: &[u8]) -> SourceResult<ModuleId> {
    let Ok(request) = std::str::from_utf8(request) else {
        return Ok(ModuleId::new(request.to_vec()));
    };
    if !is_relative_request(request) {
        return Ok(ModuleId::canonicalized(request));
    }

    let Some(requester) = requester.and_then(ModuleId::as_str) else {
        return Err(SourceError::UnresolvableRelativeRequest {
            request: request.as_bytes().to_vec(),
        });
    };
    let base = requester
        .rsplit_once("/")
        .map_or_else(String::new, |(parent, _)| parent.to_owned());
    Ok(ModuleId::new(
        ModuleName::join(&base, request).into_inner().into_bytes(),
    ))
}

/// Resolves a module request as though it was issued by `requester`.
///
/// Relative requests are joined against `requester`'s parent module; absolute
/// requests use the same canonicalization as [`resolve_request`].
pub fn resolve_request_from(
    requester: &ModuleId,
    request: impl AsRef<[u8]>,
) -> SourceResult<ModuleId> {
    resolve_request(Some(requester), request.as_ref())
}

/// Returns whether a request is relative to a context module.
#[must_use]
pub fn is_relative_request(request: &str) -> bool {
    matches!(request, "." | "..")
        || request.starts_with("./")
        || request.starts_with("../")
        || request.starts_with(".\\")
        || request.starts_with("..\\")
}

/// Resolves a path relative to a base file path.
#[must_use]
pub fn resolve_path(path: &str, base_file_path: &str) -> Option<String> {
    parent_path(base_file_path).map(|parent| normalize_path(&join_paths(&parent, path)))
}

fn join_paths(lhs: &str, rhs: &str) -> String {
    let mut result = lhs.to_owned();
    if !result.is_empty() && !result.ends_with('/') && !result.ends_with('\\') {
        result.push('/');
    }
    result.push_str(rhs);
    result
}

fn parent_path(path: &str) -> Option<String> {
    if matches!(path, "" | "." | "/") {
        return None;
    }

    #[cfg(windows)]
    if path.len() == 2 && path.ends_with(':') {
        return None;
    }

    let slash = path.rfind(['\\', '/']);
    match slash {
        Some(0) => Some("/".to_owned()),
        Some(index) => Some(path[..index].to_owned()),
        None => Some(String::new()),
    }
}

fn strip_source_extension(name: &str) -> Option<&str> {
    name.strip_suffix(".luau")
        .or_else(|| name.strip_suffix(".lua"))
}

fn is_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\'))
        || bytes
            .first()
            .is_some_and(|byte| matches!(byte, b'/' | b'\\'))
}

/// Normalizes a source path using upstream CLI path rules.
#[must_use]
pub fn normalize_path(path: &str) -> String {
    let components: Vec<_> = path.split(['\\', '/']).collect();
    let is_absolute = is_absolute_path(path);
    let mut normalized_components = Vec::new();
    let start = usize::from(is_absolute);

    for component in components.iter().skip(start) {
        match *component {
            ".." if normalized_components.is_empty() && !is_absolute => {
                normalized_components.push("..");
            }
            ".." if normalized_components.last() == Some(&"..") => {
                normalized_components.push("..");
            }
            ".." if !normalized_components.is_empty() => {
                normalized_components.pop();
            }
            ".." if is_absolute => {}
            "" | "." => {}
            component => normalized_components.push(component),
        }
    }

    let mut normalized = String::new();
    if is_absolute {
        normalized.push_str(components[0]);
        normalized.push('/');
    } else if normalized_components.first() != Some(&"..") {
        normalized.push_str("./");
    }

    normalized.push_str(&normalized_components.join("/"));
    if normalized.ends_with("..") {
        normalized.push('/');
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use super::{
        InMemorySource, ModuleId, ModuleName, Mounts, ReadContext, ReadySourceFutureExt,
        RootSource, Source, SourceEpoch, SourceError, SourceFuture, SourceMetadata, SourceProvider,
        SourceResult, chunk_load_name, normalize_path, ready, resolve_request,
        resolve_request_from,
    };

    #[test]
    fn module_id_construction_is_raw_and_canonicalization_is_explicit() {
        assert_eq!(
            ModuleId::new("./root/dep.luau").as_bytes(),
            b"./root/dep.luau"
        );
        assert_eq!(
            ModuleId::canonicalized("./root/dep.luau"),
            ModuleId::new("root/dep")
        );
        assert_eq!(
            ModuleId::new("agent://tool/search").as_bytes(),
            b"agent://tool/search"
        );
    }

    #[test]
    fn module_id_diagnostics_escape_non_utf8_bytes() {
        let id = ModuleId::from(b"bad/\xff\n\\id".as_slice());

        assert_eq!(id.to_lossy_string(), "bad/�\n\\id");
        assert_eq!(id.to_diagnostic_string(), "bad/\\xFF\\n\\\\id");
        assert_eq!(id.to_string(), "bad/\\xFF\\n\\\\id");
        assert_eq!(
            SourceError::MissingModule { id }.to_string(),
            "module 'bad/\\xFF\\n\\\\id' not found"
        );
    }

    #[test]
    fn source_preserves_source_and_default_display_identity() {
        let text = Source::text(ModuleId::new("scripts/main.luau"), "--!strict\nreturn 1");
        assert_eq!(text.id(), &ModuleId::new("scripts/main.luau"));
        assert_eq!(text.as_bytes(), b"--!strict\nreturn 1");
        assert_eq!(text.as_str(), Some("--!strict\nreturn 1"));
        assert_eq!(text.display_name(), "scripts/main.luau");

        let bytes = Source::bytes(
            ModuleId::from(b"bad/\xff".as_slice()),
            b"return \"\xff\"".as_slice(),
        );
        assert_eq!(bytes.as_bytes(), b"return \"\xff\"");
        assert_eq!(bytes.as_str(), None);
        assert_eq!(bytes.display_name(), "bad/\\xFF");

        let renamed = text.with_metadata(SourceMetadata::with_environment(
            "display/main.server.luau",
            "roblox",
        ));
        assert_eq!(renamed.display_name(), "display/main.server.luau");
        assert_eq!(renamed.load_name(), b"@display/main.server.luau");
        assert_eq!(renamed.metadata().environment.as_deref(), Some("roblox"));
    }

    #[test]
    fn source_load_names_match_host_chunk_normalization() {
        assert_eq!(chunk_load_name("scripts/main.luau"), b"@scripts/main.luau");
        assert_eq!(chunk_load_name("@scripts/main.luau"), b"@scripts/main.luau");
        assert_eq!(chunk_load_name("=inline"), b"=inline");
        assert_eq!(chunk_load_name(b"bad/\xff".as_slice()), b"@bad/\xff");

        let unit = Source::text(ModuleId::new("scripts/main.luau"), "return 1");
        assert_eq!(unit.load_name(), b"@scripts/main.luau");
    }

    #[test]
    fn module_name_normalizes_and_converts_to_canonical_id() {
        let name = ModuleName::from("./root/dep.luau");

        assert_eq!(name.as_str(), "root/dep");
        assert_eq!(name.parent(), ModuleName::from("root"));
        assert_eq!(ModuleId::from(&name), ModuleId::new("root/dep"));
    }

    #[test]
    fn normalize_path_recognizes_platform_roots_portably() {
        assert_eq!(normalize_path("/outside/secret"), "/outside/secret");
        assert_eq!(normalize_path("\\outside\\secret"), "/outside/secret");
        assert_eq!(normalize_path("C:/outside/secret"), "C:/outside/secret");
        assert_eq!(normalize_path("C:\\outside\\secret"), "C:/outside/secret");
        assert_eq!(normalize_path("C:outside\\secret"), "./C:outside/secret");
    }

    #[test]
    fn module_name_from_id_requires_utf8() {
        assert_eq!(
            ModuleName::from_id(&ModuleId::new("root/dep")).expect("valid UTF-8"),
            ModuleName::from("root/dep")
        );
        assert_eq!(
            ModuleName::from_id(&ModuleId::from(b"bad/\xff".as_slice()))
                .expect_err("non-UTF-8 runtime ids stay byte-only")
                .to_string(),
            "module id 'bad/\\xFF' is not UTF-8"
        );
    }

    #[test]
    fn resolves_canonical_text_requests() {
        assert_eq!(
            resolve_request(None, b"./a/../dep.luau"),
            Err(SourceError::UnresolvableRelativeRequest {
                request: b"./a/../dep.luau".to_vec()
            })
        );
        assert_eq!(
            resolve_request(Some(&ModuleId::new("root/main")), b"./dep.luau").expect("resolves"),
            ModuleId::new("root/dep")
        );
        assert_eq!(
            resolve_request_from(&ModuleId::new("root/main"), "./dep.luau").expect("resolves"),
            ModuleId::new("root/dep")
        );
        assert_eq!(
            resolve_request(None, b"root/dep.lua").expect("resolves"),
            ModuleId::new("root/dep")
        );
    }

    #[test]
    fn in_memory_source_reads_ready_modules() {
        let source = InMemorySource::new()
            .with_module_name("./dep.luau", "return 1")
            .with_source(
                &Source::text(ModuleId::new("tool/main"), "return 2")
                    .with_metadata(SourceMetadata::new("display/tool/main")),
            );
        let id = (source.resolve(None, b"dep"))
            .ready_only("resolving")
            .expect("resolves");
        let bytes = (source.read(&id)).ready_only("reading").expect("reads");
        assert_eq!(bytes, b"return 1");
        assert_eq!(
            source.metadata(&ModuleId::new("tool/main")),
            SourceMetadata::new("display/tool/main")
        );
    }

    #[test]
    fn ready_only_reports_pending() {
        let future: SourceFuture<ModuleId> =
            Box::pin(std::future::pending::<SourceResult<ModuleId>>());
        let error = future
            .ready_only("resolving")
            .expect_err("pending future reports an error");
        assert_eq!(
            error,
            SourceError::Pending {
                operation: "resolving"
            }
        );
    }

    #[test]
    fn in_memory_source_reports_missing_modules() {
        let source = InMemorySource::new();
        let id = ModuleId::new("missing");

        assert_eq!(
            (source.read(&id))
                .ready_only("reading")
                .expect_err("missing module"),
            SourceError::MissingModule { id }
        );
    }

    #[test]
    fn in_memory_source_metadata_fallback_and_epoch_bump() {
        let mut source = InMemorySource::new();
        let id = ModuleId::new("dep");
        assert_eq!(source.epoch(), 0);
        assert_eq!(source.metadata(&id), SourceMetadata::new("dep"));

        source.insert(id.clone(), "return 1");
        assert_eq!(source.epoch(), 1);
        source.set_metadata(
            id.clone(),
            SourceMetadata::with_environment("display/dep", "roblox"),
        );
        assert_eq!(source.epoch(), 2);
        assert_eq!(
            source.metadata(&id),
            SourceMetadata::with_environment("display/dep", "roblox")
        );

        source.insert_with_metadata(
            ModuleId::new("other"),
            "return 2",
            SourceMetadata::new("display/other"),
        );
        assert_eq!(source.epoch(), 3);
        assert_eq!(
            source.metadata(&ModuleId::new("other")),
            SourceMetadata::new("display/other")
        );
    }

    #[test]
    fn in_memory_source_aliases_resolution_reads_and_metadata() {
        let source = InMemorySource::new()
            .with_module(ModuleId::new("core/json"), "return {}")
            .with_metadata(
                ModuleId::new("core/json"),
                SourceMetadata::new("display/core/json"),
            )
            .with_alias(ModuleId::new("@core/json"), ModuleId::new("core/json"));

        let id = (source.resolve(None, b"@core/json"))
            .ready_only("resolving alias")
            .expect("alias resolves");
        assert_eq!(id, ModuleId::new("core/json"));
        assert_eq!(
            (source.read(&ModuleId::new("@core/json")))
                .ready_only("reading alias")
                .expect("alias reads"),
            b"return {}".to_vec()
        );
        assert_eq!(
            source.metadata(&ModuleId::new("@core/json")),
            SourceMetadata::new("display/core/json")
        );
    }

    #[test]
    fn mounted_source_dispatches_prefixed_requests_and_relative_reads() {
        let user = Arc::new(
            InMemorySource::new()
                .with_module(ModuleId::new("root/main"), "return require('./dep')")
                .with_module(ModuleId::new("root/dep"), "return 1")
                .with_metadata(
                    ModuleId::new("root/dep"),
                    SourceMetadata::new("display/root/dep"),
                ),
        );
        let source = Mounts::builder()
            .with_mount(ModuleId::new("@user"), user)
            .build();

        let main = (source.resolve(None, b"@user/root/main"))
            .ready_only("resolving main")
            .expect("main resolves");
        assert_eq!(main, ModuleId::new("@user/root/main"));

        let dep = (source.resolve(Some(&main), b"./dep"))
            .ready_only("resolving relative dep")
            .expect("relative dep resolves inside the requester mount");
        assert_eq!(dep, ModuleId::new("@user/root/dep"));
        assert_eq!(
            (source.read_request(ReadContext::with_requester(&dep, Some(&main))))
                .ready_only("reading dep",)
                .expect("dep reads"),
            b"return 1".to_vec()
        );
        assert_eq!(
            source.metadata(&dep),
            SourceMetadata::new("@user/display/root/dep")
        );
    }

    #[test]
    fn mounted_source_rejects_bare_unknown_and_non_utf8_requests() {
        let user =
            Arc::new(InMemorySource::new().with_module(ModuleId::new("root/main"), "return 1"));
        let source = Mounts::builder()
            .with_mount(ModuleId::new("@user"), user)
            .build();

        assert_eq!(
            (source.resolve(None, b"root/main"))
                .ready_only("resolving bare")
                .expect_err("bare names do not fall through"),
            SourceError::MissingModule {
                id: ModuleId::new("root/main")
            }
        );
        assert_eq!(
            (source.resolve(None, b"@project/main"))
                .ready_only("resolving unmounted")
                .expect_err("unknown mounts are missing"),
            SourceError::MissingModule {
                id: ModuleId::new("@project/main")
            }
        );
        let error = (source.resolve(None, b"@user/\xff"))
            .ready_only("resolving non-UTF-8 mounted request")
            .expect_err("mounted requests must be UTF-8");
        assert!(error.to_string().contains("not UTF-8"));
    }

    #[test]
    fn mounted_source_prefixes_instance_keys_and_folds_epochs() {
        let left = Arc::new(InMemorySource::new().with_module(ModuleId::new("dep"), "return 1"));
        let right = Arc::new(InMemorySource::new().with_module(ModuleId::new("dep"), "return 2"));
        let supplied_epoch = SourceEpoch::new(7);
        let mut source = Mounts::builder().build();
        let left_epoch = SourceEpoch::default();
        source.insert(ModuleId::new("@left"), left, left_epoch.clone());
        source.insert(ModuleId::new("@right"), right, supplied_epoch.clone());

        let left_id = ModuleId::new("@left/dep");
        let right_id = ModuleId::new("@right/dep");
        assert_ne!(
            source.instance_key(ReadContext::new(&left_id)),
            source.instance_key(ReadContext::new(&right_id)),
            "two mounts resolving the same child id must not share a VM export cache key"
        );
        let mounted_epoch = source.epoch();
        assert_ne!(mounted_epoch, Mounts::builder().build().epoch());
        assert_eq!(left_epoch.bump(), 1);
        assert_ne!(source.epoch(), mounted_epoch);
        supplied_epoch.set(8);
        assert_ne!(source.epoch(), mounted_epoch);
    }

    #[test]
    fn mounted_source_observation_preserves_origin_and_prefixes_identity() {
        struct PhysicalSource;

        impl SourceProvider for PhysicalSource {
            fn resolve(
                &self,
                requester: Option<&ModuleId>,
                request: &[u8],
            ) -> SourceFuture<ModuleId> {
                ready(resolve_request(requester, request))
            }

            fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
                ready(Ok(
                    format!("return {:?}", id.to_diagnostic_string()).into_bytes()
                ))
            }

            fn read_observation(
                &self,
                request: ReadContext<'_>,
            ) -> SourceFuture<super::SourceRead> {
                let source = Source::text(request.id().clone(), "return 1")
                    .with_metadata(SourceMetadata::new("physical/dep.luau"));
                ready(Ok(super::SourceRead::new(
                    source,
                    super::InstanceKey::new(request.id().clone(), request.requester().cloned()),
                    9,
                    Some(PathBuf::from("/physical/dep.luau")),
                )))
            }

            fn epoch(&self) -> u64 {
                9
            }
        }

        let source = Mounts::builder()
            .with_mount(ModuleId::new("@physical"), Arc::new(PhysicalSource))
            .build();
        let id = ModuleId::new("@physical/dep");
        let requester = ModuleId::new("@physical/main");
        let expected_epoch = source.epoch();
        let observed = source
            .read_observation(ReadContext::with_requester(&id, Some(&requester)))
            .ready_only("observing mounted source")
            .expect("mounted source observation");

        assert_eq!(observed.source().id(), &id);
        assert_eq!(
            observed.source().display_name(),
            "@physical/physical/dep.luau"
        );
        assert_eq!(
            observed.instance_key(),
            &super::InstanceKey::new(id, Some(requester))
        );
        assert_eq!(observed.epoch(), expected_epoch);
        let expected_origin = PathBuf::from("/physical/dep.luau");
        assert_eq!(observed.origin(), Some(expected_origin.as_path()));
    }

    #[test]
    fn root_overlay_serves_root_and_delegates_relative_requests() {
        let delegate = Arc::new(
            InMemorySource::new()
                .with_module(ModuleId::new("app/main"), "return require('./dep')")
                .with_module(ModuleId::new("app/dep"), "return 1")
                .with_metadata(ModuleId::new("app/dep"), SourceMetadata::new("display/dep")),
        );
        let source = RootSource::new(ModuleId::new("__root__"), "return require('./dep')")
            .with_root_name("Script")
            .with_display_name("script.luau")
            .with_delegate(delegate)
            .with_root_requester(ModuleId::new("app/main"));

        let root = ModuleId::new("__root__");
        let dep = (source.resolve(Some(&root), b"./dep"))
            .ready_only("resolving root-relative dep")
            .expect("root-relative dep resolves through delegate requester");

        assert_eq!(source.root_name(), ModuleName::from("Script"));
        assert_eq!(dep, ModuleId::new("app/dep"));
        assert_eq!(
            (source.read(&root))
                .ready_only("reading root")
                .expect("root reads"),
            b"return require('./dep')".to_vec()
        );
        assert_eq!(source.metadata(&root), SourceMetadata::new("script.luau"));
        assert_eq!(source.metadata(&dep), SourceMetadata::new("display/dep"));
    }

    #[test]
    fn root_overlay_rejects_delegate_root_id_collision() {
        let delegate = Arc::new(
            InMemorySource::new().with_alias(ModuleId::new("dep"), ModuleId::new("__root__")),
        );
        let source = RootSource::new(ModuleId::new("__root__"), "return 1").with_delegate(delegate);

        let error = (source.resolve(None, b"dep"))
            .ready_only("resolving collision")
            .expect_err("delegate cannot resolve the synthetic root id");

        assert!(error.to_string().contains("reserved for the root overlay"));
    }

    #[test]
    fn root_overlay_resolves_canonical_root_before_delegate() {
        struct RejectingDelegate;

        impl SourceProvider for RejectingDelegate {
            fn resolve(
                &self,
                _requester: Option<&ModuleId>,
                _request: &[u8],
            ) -> SourceFuture<ModuleId> {
                panic!("canonical root resolution must not reach the delegate")
            }

            fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
                ready(Err(SourceError::MissingModule { id: id.clone() }))
            }
        }

        let source = RootSource::new(ModuleId::new("root/main"), "return require('root/main')")
            .with_delegate(Arc::new(RejectingDelegate));

        assert_eq!(
            source
                .resolve(None, b"root/main.luau")
                .ready_only("resolving canonical root")
                .expect("canonical root resolves"),
            ModuleId::new("root/main")
        );
    }

    #[test]
    fn root_overlay_resolves_root_relative_cycle_to_itself() {
        let delegate = Arc::new(
            InMemorySource::new()
                .with_module(ModuleId::new("app/main"), "return 2")
                .with_module(ModuleId::new("app/dep"), "return 1"),
        );
        let root = ModuleId::new("app/main");
        let source = RootSource::new(root.clone(), "return require('./main')")
            .with_delegate(delegate)
            .with_root_requester(root.clone());

        assert_eq!(
            source
                .resolve(Some(&root), b"./main")
                .ready_only("resolving root cycle")
                .expect("root cycle resolves to the overlay"),
            root
        );
    }

    #[test]
    fn immutable_source_default_observation_is_complete() {
        struct StaticSource;

        impl super::SyncSourceProvider for StaticSource {
            fn resolve_sync(
                &self,
                requester: Option<&ModuleId>,
                request: &[u8],
            ) -> SourceResult<ModuleId> {
                resolve_request(requester, request)
            }

            fn read_sync(&self, id: &ModuleId) -> SourceResult<Vec<u8>> {
                Ok(format!("return {:?}", id.to_diagnostic_string()).into_bytes())
            }

            fn metadata(&self, id: &ModuleId) -> SourceMetadata {
                SourceMetadata::new(format!("display/{id}"))
            }
        }

        let source = StaticSource;
        let id = ModuleId::new("dep");
        let read = source
            .read_observation(ReadContext::new(&id))
            .ready_only("observing immutable source")
            .expect("source observation");

        assert_eq!(read.source().id(), &id);
        assert_eq!(read.source().as_bytes(), b"return \"dep\"");
        assert_eq!(read.source().display_name(), "display/dep");
        assert_eq!(read.instance_key(), &super::InstanceKey::shared(id));
        assert_eq!(read.epoch(), 0);
        assert_eq!(read.origin(), None);
    }

    #[test]
    fn root_overlay_lets_delegate_handle_root_relative_requests_without_requester() {
        struct CwdRelativeDelegate;

        impl SourceProvider for CwdRelativeDelegate {
            fn resolve(
                &self,
                requester: Option<&ModuleId>,
                request: &[u8],
            ) -> SourceFuture<ModuleId> {
                assert!(requester.is_none());
                assert_eq!(request, b"./dep");
                ready(Ok(ModuleId::new("cwd/dep")))
            }

            fn read(&self, id: &ModuleId) -> SourceFuture<Vec<u8>> {
                ready(Err(SourceError::MissingModule { id: id.clone() }))
            }
        }

        let delegate = Arc::new(CwdRelativeDelegate);
        let source = RootSource::new(ModuleId::new("__root__"), "return require('./dep')")
            .with_delegate(delegate);
        let root = ModuleId::new("__root__");

        let dep = (source.resolve(Some(&root), b"./dep"))
            .ready_only("resolving delegated cwd-relative request")
            .expect("delegate decides how to resolve cwd-relative root request");

        assert_eq!(dep, ModuleId::new("cwd/dep"));
    }

    #[test]
    fn root_source_accepts_sync_delegate_and_epoch() {
        let delegate =
            Arc::new(InMemorySource::new().with_module(ModuleId::new("app/dep"), "return 1"));
        let source = RootSource::new(ModuleId::new("__root__"), "return require('./dep')")
            .with_delegate(delegate.clone())
            .with_root_requester(ModuleId::new("app/main"));
        let root = ModuleId::new("__root__");

        assert_eq!(
            (source.resolve(Some(&root), b"./dep"))
                .ready_only("resolving sync delegate")
                .expect("sync root-relative dep resolves"),
            ModuleId::new("app/dep")
        );
        assert_eq!(
            source.epoch(),
            super::SyncSourceProvider::epoch(delegate.as_ref()),
        );
    }

    #[test]
    fn sync_module_source_gets_async_module_source_impl() {
        struct StaticSource;

        impl super::SyncSourceProvider for StaticSource {
            fn resolve_sync(
                &self,
                requester: Option<&ModuleId>,
                request: &[u8],
            ) -> SourceResult<ModuleId> {
                resolve_request(requester, request)
            }

            fn read_sync(&self, id: &ModuleId) -> SourceResult<Vec<u8>> {
                if id == &ModuleId::new("dep") {
                    Ok(b"return 1".to_vec())
                } else {
                    Err(SourceError::MissingModule { id: id.clone() })
                }
            }
        }

        let source = StaticSource;
        let id = (source.resolve(None, b"dep"))
            .ready_only("resolving sync source")
            .expect("sync source resolves through SourceProvider");
        assert_eq!(id, ModuleId::new("dep"));
        assert_eq!(
            (source.read_request(ReadContext::with_requester(
                &id,
                Some(&ModuleId::new("requester")),
            )))
            .ready_only("reading sync source",)
            .expect("sync source reads through SourceProvider"),
            b"return 1".to_vec()
        );
    }
}
