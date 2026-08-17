//! Stable low-level API shared by the Ruau VM and extension hosts.
//!
//! `ruau` mounts this whole crate as `ruau::vm`. Use it when native
//! modules, host callbacks, tests, or engine-facing support code need to name
//! VM boundary types directly. Most embedders still start with `ruau::vm` and
//! reach for `ruau::vm` only at the host/native-module boundary.
//!
//! Host callbacks receive borrowed [`HostValue`]s and return owned
//! [`OwnedValue`]s. Raw heap handles are represented by [`RawValue`] and
//! [`RawGc`] for engine code; they are not a host return format.

use std::{any::Any, borrow::Cow, future::Future, marker::PhantomData, pin::Pin, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A per-VM nonce stamped into every handle. A handle minted by one VM is
/// rejected by another even on an index-and-generation collision.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Deserialize, Serialize)]
pub struct HeapId(pub u64);

/// Opaque zero-size marker kinds distinguishing what a handle points at. The
/// engine maps a handle to its private arena storage; the ABI names no engine
/// type.
pub mod marker {
    /// Interned string.
    #[derive(Clone, Copy, Debug)]
    pub struct Str;
    /// Table.
    #[derive(Clone, Copy, Debug)]
    pub struct Table;
    /// Closure.
    #[derive(Clone, Copy, Debug)]
    pub struct Closure;
    /// Host userdata.
    #[derive(Clone, Copy, Debug)]
    pub struct Userdata;
    /// Thread / coroutine.
    #[derive(Clone, Copy, Debug)]
    pub struct Thread;
    /// Byte buffer.
    #[derive(Clone, Copy, Debug)]
    pub struct Buffer;
}

/// Persistent, unbranded handle: a generational arena index plus the owning
/// heap's nonce. Fills `StackStore` slots and table storage — anything that
/// outlives a single borrow of the VM.
///
/// This is an engine/embedder handle, not an unforgeable capability. Resolving
/// a handle checks that the named slot is live in the owning heap, and the host
/// return path is deliberately narrower: synchronous and asynchronous host calls
/// carry [`OwnedValue`] or registry refs minted by the VM, never raw handles.
/// Do not hand `RawGc` values to hostile tenants or treat the raw setup APIs
/// that accept them as a tenant-security boundary.
///
/// `Copy`/`Clone`/`Debug` are implemented by hand rather than derived: a handle
/// is just indices, so it is always `Copy` regardless of whether the object type
/// `T` is.
pub struct RawGc<T> {
    index: u32,
    generation: u32,
    heap: HeapId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Serialize for RawGc<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (self.index, self.generation, self.heap).serialize(serializer)
    }
}

impl<'de, T> Deserialize<'de> for RawGc<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (index, generation, heap) = <(u32, u32, HeapId)>::deserialize(deserializer)?;
        Ok(Self::from_parts(index, generation, heap))
    }
}

impl<T> Clone for RawGc<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for RawGc<T> {}

impl<T> PartialEq for RawGc<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.generation == other.generation && self.heap == other.heap
    }
}

impl<T> Eq for RawGc<T> {}

impl<T> std::fmt::Debug for RawGc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawGc")
            .field("index", &self.index)
            .field("generation", &self.generation)
            .field("heap", &self.heap)
            .finish()
    }
}

impl<T> RawGc<T> {
    /// Constructs a raw handle from its parts. This is a **trusted handle-minting
    /// primitive**: it fabricates a handle the engine will later trust and
    /// dereference, so a caller that mints an index/generation the heap did not
    /// issue can forge a stale or cross-VM reference. It is safe (not `unsafe`)
    /// only because every deref re-checks the generation and heap nonce, turning a
    /// forged handle into a clean rejection rather than a memory hazard. It exists
    /// for two trusted callers: the engine in a sibling crate minting handles
    /// across the ABI, and focused tests that exercise stale/cross-VM/forged-handle
    /// rejection. A host *function return* cannot carry a raw handle — the
    /// [`HostCall`] contract hands the engine only [`OwnedValue`]s — so a host
    /// callee cannot forge one through its return. The raw [`RawValue`] surface
    /// that remains is on the low-level `Vm::call`/`call_function` entry points; a
    /// general embedder should not mint handles, and the safe `Scope`/runner
    /// profiles supersede those raw entry points.
    #[doc(hidden)] // Engine-internal handle minting; not an embedder API.
    #[must_use]
    pub fn from_parts(index: u32, generation: u32, heap: HeapId) -> Self {
        Self {
            index,
            generation,
            heap,
            _marker: PhantomData,
        }
    }

    /// The arena index.
    #[must_use]
    pub fn index(self) -> u32 {
        self.index
    }

    /// The generation stamp, checked on deref to reject a stale handle.
    #[must_use]
    pub fn generation(self) -> u32 {
        self.generation
    }

    /// The owning heap nonce, checked on deref to reject a cross-VM handle.
    #[must_use]
    pub fn heap(self) -> HeapId {
        self.heap
    }
}

/// Public borrow-view handle, branded by the VM borrow lifetime so the compiler
/// forbids cross-VM and cross-await use; the heap nonce is the runtime backstop.
pub struct Gc<'vm, T> {
    raw: RawGc<T>,
    _vm: PhantomData<&'vm ()>,
}

impl<T> Clone for Gc<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Gc<'_, T> {}

impl<T> std::fmt::Debug for Gc<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gc").field("raw", &self.raw).finish()
    }
}

/// Raw engine conversions for [`Gc`].
///
/// This is a low-level engine bridge. Ordinary host code should prefer
/// [`HostContext::pin_arg`] and [`OwnedValue::Pinned`] when it needs to return
/// a heap value.
pub trait GcRawExt<T>: Sized {
    /// Views a raw handle for the duration of a VM borrow.
    #[must_use]
    fn from_raw(raw: RawGc<T>) -> Self;

    /// The underlying persistent handle.
    #[must_use]
    fn raw(self) -> RawGc<T>;
}

impl<'vm, T> GcRawExt<T> for Gc<'vm, T> {
    fn from_raw(raw: RawGc<T>) -> Self {
        Self {
            raw,
            _vm: PhantomData,
        }
    }

    fn raw(self) -> RawGc<T> {
        self.raw
    }
}

/// The value shape with persistent, unbranded handles. Identical in shape to
/// [`HostValue`], which is the branded borrow-view.
///
/// `PartialEq` is structural raw equality (same tag, same bits or handle
/// identity) — `rawequal` semantics, not the `==` metamethod. It is not `Eq`
/// because it carries `f64`.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
pub enum RawValue {
    /// `nil`.
    Nil,
    /// Boolean.
    Boolean(bool),
    /// IEEE-754 double (`LUA_TNUMBER`).
    Number(f64),
    /// 64-bit integer (`LUA_TINTEGER`, the pinned revision's native ints).
    Integer(i64),
    /// Three-lane vector (`LUA_VECTOR_SIZE == 3`).
    Vector([f32; 3]),
    /// Opaque host token, not a raw pointer.
    LightUserdata {
        /// Host-defined payload.
        handle: u32,
        /// Host-defined tag.
        tag: u8,
    },
    /// Interned string.
    String(RawGc<marker::Str>),
    /// Table.
    Table(RawGc<marker::Table>),
    /// Closure.
    Function(RawGc<marker::Closure>),
    /// Host userdata.
    Userdata(RawGc<marker::Userdata>),
    /// Thread / coroutine.
    Thread(RawGc<marker::Thread>),
    /// Byte buffer.
    Buffer(RawGc<marker::Buffer>),
}

/// The value shape a host receives: a thin borrow-view over a [`RawValue`],
/// branded by the VM borrow so a `'static` async future cannot hold one.
#[derive(Clone, Copy, Debug)]
pub enum HostValue<'vm> {
    /// `nil`.
    Nil,
    /// Boolean.
    Boolean(bool),
    /// IEEE-754 double.
    Number(f64),
    /// 64-bit integer.
    Integer(i64),
    /// Three-lane vector.
    Vector([f32; 3]),
    /// Opaque host token.
    LightUserdata {
        /// Host-defined payload.
        handle: u32,
        /// Host-defined tag.
        tag: u8,
    },
    /// Interned string.
    String(Gc<'vm, marker::Str>),
    /// Table.
    Table(Gc<'vm, marker::Table>),
    /// Closure.
    Function(Gc<'vm, marker::Closure>),
    /// Host userdata.
    Userdata(Gc<'vm, marker::Userdata>),
    /// Thread / coroutine.
    Thread(Gc<'vm, marker::Thread>),
    /// Byte buffer.
    Buffer(Gc<'vm, marker::Buffer>),
}

/// Raw engine conversion for [`HostValue`].
///
/// This is a low-level engine bridge. Supported host callbacks receive
/// [`HostValue`]s from [`HostContext`]; they should not need to brand raw values
/// themselves.
pub trait HostValueRawExt<'vm>: Sized {
    /// Brands a [`RawValue`] as a borrow-view for the duration of a VM borrow.
    /// The load-bearing guarantees are elsewhere: the async return type
    /// ([`OwnedValue`]) carries no raw handle, and the engine validates every
    /// synchronous host result. See [`HostContext`].
    #[must_use]
    fn from_raw(raw: RawValue) -> Self;
}

impl<'vm> HostValueRawExt<'vm> for HostValue<'vm> {
    fn from_raw(raw: RawValue) -> Self {
        match raw {
            RawValue::Nil => Self::Nil,
            RawValue::Boolean(b) => Self::Boolean(b),
            RawValue::Number(n) => Self::Number(n),
            RawValue::Integer(i) => Self::Integer(i),
            RawValue::Vector(v) => Self::Vector(v),
            RawValue::LightUserdata { handle, tag } => Self::LightUserdata { handle, tag },
            RawValue::String(g) => Self::String(Gc::from_raw(g)),
            RawValue::Table(g) => Self::Table(Gc::from_raw(g)),
            RawValue::Function(g) => Self::Function(Gc::from_raw(g)),
            RawValue::Userdata(g) => Self::Userdata(Gc::from_raw(g)),
            RawValue::Thread(g) => Self::Thread(Gc::from_raw(g)),
            RawValue::Buffer(g) => Self::Buffer(Gc::from_raw(g)),
        }
    }
}

#[derive(Debug)]
struct RegistryToken;

/// A driver-owned registry pin keeping a Lua value rooted across an await
/// (`lua_ref`). The future holds only this opaque token, never a `Gc`.
///
/// The public slot and generation accessors are diagnostics only. The engine
/// also validates a private token identity stored in the registry slot, so a
/// host cannot forge a live pin by constructing another value with the same
/// numeric parts.
#[derive(Clone)]
pub struct RegistryRef {
    slot: u32,
    generation: u32,
    heap: HeapId,
    token: Arc<RegistryToken>,
}

impl std::fmt::Debug for RegistryRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryRef")
            .field("slot", &self.slot)
            .field("generation", &self.generation)
            .field("heap", &self.heap)
            .finish_non_exhaustive()
    }
}

impl PartialEq for RegistryRef {
    fn eq(&self, other: &Self) -> bool {
        self.slot == other.slot
            && self.generation == other.generation
            && self.heap == other.heap
            && Arc::ptr_eq(&self.token, &other.token)
    }
}

impl Eq for RegistryRef {}

impl RegistryRef {
    /// Constructs a pin token from its visible parts (engine-internal).
    ///
    /// A token built this way is unique even when the numeric parts match an
    /// existing pin. Only the exact token the engine stored in the registry slot
    /// can resolve, so external callers cannot forge a live pin with this
    /// constructor.
    #[doc(hidden)] // Engine-internal handle minting; not an embedder API.
    #[must_use]
    pub(crate) fn from_parts(slot: u32, generation: u32, heap: HeapId) -> Self {
        Self {
            slot,
            generation,
            heap,
            token: Arc::new(RegistryToken),
        }
    }

    /// The registry slot.
    #[must_use]
    pub fn slot(&self) -> u32 {
        self.slot
    }

    /// The slot generation, checked when the pin is materialized or released.
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The owning heap nonce.
    #[must_use]
    pub(crate) fn heap(&self) -> HeapId {
        self.heap
    }
}

/// Category of an uncaught failure.
///
/// Cancellation, deadline, and panic poison are fatal inside the VM and cannot
/// be caught by `pcall` or `xpcall`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeErrorKind {
    /// An ordinary script runtime error.
    #[default]
    Runtime,
    /// A memory-cap or allocation failure. Catchable, like Lua.
    Memory,
    /// The request was cancelled. Fatal.
    Cancelled,
    /// A wall-clock deadline was exceeded. Fatal.
    Deadline,
    /// The VM refused work because a contained panic already poisoned it, or
    /// because this call caught the panic and poisoned the VM. Fatal.
    PanicPoison,
    /// An `xpcall` message handler raised while replacing the original error.
    /// Catchable if surfaced as an error; builtin `xpcall` still returns the
    /// Lua-visible `"error in error handling"` value.
    HandlerFailure,
    /// A runtime `require` request could not be resolved or loaded from the
    /// configured module source. Catchable, like an ordinary runtime error.
    UnresolvedRequire,
}

impl RuntimeErrorKind {
    /// Whether `pcall`/`xpcall` may catch an error of this kind. Cancellation,
    /// deadline, and panic poison are fatal so a tenant cannot defeat
    /// termination or reuse a poisoned VM.
    #[must_use]
    pub fn catchable(self) -> bool {
        matches!(
            self,
            Self::Runtime | Self::Memory | Self::HandlerFailure | Self::UnresolvedRequire
        )
    }
}

/// Low-level script-error carrier used by raw VM entry points.
///
/// The `error` value is a heap [`RawValue`], so callers must materialize it
/// before the heap goes away.
#[derive(Clone, Debug)]
pub struct Unwind {
    /// The error object (usually a string or table).
    pub error: RawValue,
    /// The failure category, for a runner's typed metric.
    pub kind: RuntimeErrorKind,
}

/// Error returned by a synchronous host function.
///
/// The error value is owned data, never a raw heap handle.
#[derive(Clone, Debug)]
pub struct HostUnwind {
    /// The owned error value — a forged raw handle is unrepresentable here.
    pub error: OwnedValue,
    /// The failure category, for a runner's typed metric.
    pub kind: RuntimeErrorKind,
    /// Optional script-visible table fields for structured host errors.
    ///
    /// When non-empty, the engine surfaces a table at the Lua error boundary,
    /// seeded with the canonical `message` field from [`error`](Self::error) when
    /// it is a byte string, then extended by these caller-provided fields.
    pub script_fields: Vec<ScriptErrorField>,
}

/// Typed, host-only payload attached to a host-raised error.
///
/// Scripts never observe this payload. `PartialEq` compares payload identity.
#[derive(Clone)]
pub struct HostPayload(Arc<dyn Any + Send + Sync>);

impl HostPayload {
    /// Wraps a typed host value as error freight.
    #[must_use]
    pub fn new(payload: impl Any + Send + Sync) -> Self {
        Self(Arc::new(payload))
    }

    /// The payload, downcast to its concrete type; `None` if `T` is not the
    /// type the host attached.
    #[must_use]
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}

impl PartialEq for HostPayload {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for HostPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostPayload(..)")
    }
}

/// Error raised by an asynchronous host function while the VM is not borrowed.
#[derive(Clone, Debug)]
pub struct HostError {
    /// Human-facing message.
    pub message: String,
    /// The failure category, for catchability and runner metrics.
    pub kind: RuntimeErrorKind,
    /// Typed host freight riding the raised error to the exit surfaces;
    /// scripts never observe it.
    pub payload: Option<HostPayload>,
    /// Optional script-visible table fields for structured host errors.
    ///
    /// String-only callers leave this empty. When present, `pcall`/`xpcall` see
    /// a Lua table instead of the message string.
    pub script_fields: Vec<ScriptErrorField>,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HostError {}

/// Owned values returned by an async host function.
#[derive(Clone, Debug, Default)]
pub struct HostReturn {
    /// The values to materialize, in order.
    pub values: Vec<OwnedValue>,
}

/// A single owned return value: an immediate scalar, owned bytes the driver
/// interns, or a registry pin into the VM heap.
#[derive(Clone, Debug)]
pub enum OwnedValue {
    /// `nil`.
    Nil,
    /// Boolean.
    Boolean(bool),
    /// Number.
    Number(f64),
    /// Integer.
    Integer(i64),
    /// Three-lane vector.
    Vector([f32; 3]),
    /// Opaque host token.
    LightUserdata {
        /// Host-defined payload.
        handle: u32,
        /// Host-defined tag.
        tag: u8,
    },
    /// Owned bytes, interned into a string on materialization.
    Bytes(Vec<u8>),
    /// A pinned heap value.
    Pinned(RegistryRef),
}

impl OwnedValue {
    /// Luau's ordinary type name for this owned value.
    ///
    /// A pinned heap value has no heap borrow here, so it reports the generic
    /// `"pinned"` kind; materializing it in a VM recovers the precise Lua kind.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Nil => "nil",
            Self::Boolean(_) => "boolean",
            Self::Number(_) | Self::Integer(_) => "number",
            Self::Vector(_) => "vector",
            Self::LightUserdata { .. } => "userdata",
            Self::Bytes(_) => "string",
            Self::Pinned(_) => "pinned",
        }
    }

    /// Conservative display text for this owned value.
    ///
    /// Strings return their bytes lossily decoded as UTF-8. Scalar values use
    /// Luau's scalar spelling. A pinned heap value has no heap borrow here, so
    /// it reports the generic `"pinned"` kind; materializing it in a VM
    /// recovers the precise Lua kind.
    #[must_use]
    pub fn display_lua(&self) -> String {
        match self {
            Self::Nil => "nil".to_owned(),
            Self::Boolean(true) => "true".to_owned(),
            Self::Boolean(false) => "false".to_owned(),
            Self::Number(value) => crate::vmutils::number_to_string(*value),
            Self::Integer(value) => value.to_string(),
            Self::Vector(value) => value
                .iter()
                .map(|component| crate::vmutils::number_to_string(f64::from(*component)))
                .collect::<Vec<_>>()
                .join(", "),
            Self::LightUserdata { .. } => "userdata".to_owned(),
            Self::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Self::Pinned(_) => "pinned".to_owned(),
        }
    }
}

impl From<bool> for OwnedValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i64> for OwnedValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<f64> for OwnedValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<Vec<u8>> for OwnedValue {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

impl From<String> for OwnedValue {
    fn from(value: String) -> Self {
        Self::Bytes(value.into_bytes())
    }
}

impl From<&'static str> for OwnedValue {
    fn from(value: &'static str) -> Self {
        Self::Bytes(value.as_bytes().to_vec())
    }
}

/// One script-visible field on a structured host error.
///
/// The name becomes a string key in the Lua error table; the value is
/// materialized through the same owned-value boundary as host returns.
#[derive(Clone, Debug)]
pub struct ScriptErrorField {
    /// String key in the Lua error table.
    pub name: Cow<'static, str>,
    /// Owned field value.
    pub value: OwnedValue,
}

impl ScriptErrorField {
    /// Builds one structured error field.
    #[must_use]
    pub fn new(name: impl Into<Cow<'static, str>>, value: impl Into<OwnedValue>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// A build-time module value installed into a VM environment.
#[derive(Clone, Debug)]
pub enum ModuleValue {
    /// `nil`.
    Nil,
    /// Boolean.
    Boolean(bool),
    /// Number.
    Number(f64),
    /// Integer.
    Integer(i64),
    /// Opaque host token.
    LightUserdata {
        /// Host-defined payload.
        handle: u32,
        /// Host-defined tag.
        tag: u8,
    },
    /// Owned bytes, interned into a Lua string when the module is installed.
    Bytes(Vec<u8>),
    /// A dense array-valued constant table.
    Array(ModuleArray),
    /// A string-keyed constant table.
    Table(ModuleTable),
}

impl From<bool> for ModuleValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i64> for ModuleValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<f64> for ModuleValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<Vec<u8>> for ModuleValue {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

impl From<&str> for ModuleValue {
    fn from(value: &str) -> Self {
        Self::Bytes(value.as_bytes().to_vec())
    }
}

impl From<ModuleArray> for ModuleValue {
    fn from(value: ModuleArray) -> Self {
        Self::Array(value)
    }
}

impl From<ModuleTable> for ModuleValue {
    fn from(value: ModuleTable) -> Self {
        Self::Table(value)
    }
}

/// A dense array-valued constant table installed by a [`NativeModule`].
///
/// Installed arrays carry the protected JSON-array marker so their shape is
/// preserved when they are empty or cross an owned JSON boundary.
#[derive(Clone, Debug, Default)]
pub struct ModuleArray {
    /// Values installed at one-based sequence indexes.
    pub values: Vec<ModuleValue>,
}

impl ModuleArray {
    /// Builds an empty array.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one value and returns the array for builder-style construction.
    #[must_use]
    pub fn value(mut self, value: impl Into<ModuleValue>) -> Self {
        self.values.push(value.into());
        self
    }
}

/// One string-keyed entry in a [`ModuleTable`].
#[derive(Clone, Debug)]
pub struct ModuleTableEntry {
    /// Entry name.
    pub name: Cow<'static, str>,
    /// Entry value.
    pub value: ModuleValue,
}

impl ModuleTableEntry {
    /// Builds a named table entry.
    #[must_use]
    pub fn new(name: impl Into<Cow<'static, str>>, value: impl Into<ModuleValue>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// A string-keyed constant table installed by a [`NativeModule`].
#[derive(Clone, Debug, Default)]
pub struct ModuleTable {
    /// Entries installed into the table.
    pub entries: Vec<ModuleTableEntry>,
}

impl ModuleTable {
    /// Builds an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one entry and returns the table for builder-style construction.
    #[must_use]
    pub fn entry(
        mut self,
        name: impl Into<Cow<'static, str>>,
        value: impl Into<ModuleValue>,
    ) -> Self {
        self.entries.push(ModuleTableEntry::new(name, value));
        self
    }
}

/// A boxed future, the async half of a host call. `Send` on native targets
/// (the driver may run on a multi-threaded executor); on wasm32 the executor
/// is single-threaded and JS-backed futures are `!Send`, so the bound drops.
#[cfg(not(target_arch = "wasm32"))]
pub type HostFuture = Pin<Box<dyn Future<Output = Result<HostReturn, HostError>> + Send + 'static>>;
/// A boxed future, the async half of a host call (wasm: no `Send` bound; the
/// executor is single-threaded and JS-backed futures are `!Send`).
#[cfg(target_arch = "wasm32")]
pub type HostFuture = Pin<Box<dyn Future<Output = Result<HostReturn, HostError>> + 'static>>;

/// The outcome of invoking a [`HostFunction`]: synchronous values, or a pending
/// future the driver awaits.
pub enum HostCall {
    /// Synchronous result. The values are the same owned, heap-dereference-free
    /// [`OwnedValue`] form an async return uses, so a host *cannot represent* a
    /// raw `RawGc` in a synchronous return either: a forged, stale, or cross-VM
    /// handle is unrepresentable, not merely rejected. To return a heap value the
    /// host pins an argument by position ([`HostContext::pin_arg`] →
    /// [`OwnedValue::Pinned`]), which the engine mints and validates; bytes return
    /// as [`OwnedValue::Bytes`] and intern through the accounted heap. The error
    /// object is a [`HostUnwind`], whose value is owned too — so the whole
    /// synchronous return, success *and* error, is structurally forgery-proof.
    Ready(Result<Vec<OwnedValue>, HostUnwind>),
    /// An asynchronous host call the driver awaits off the VM borrow.
    Pending(HostFuture),
}

/// A host-provided function callable from Lua. Leaf host calls only; control
/// primitives like `pcall` are engine builtins, not `HostFunction`s.
pub trait HostFunction: Send + Sync {
    /// Runs the synchronous part of the call and returns its outcome.
    fn call(&self, ctx: &mut dyn HostContext) -> HostCall;
}

/// The controlled bridge a [`HostFunction`] uses during its synchronous part: it
/// reads arguments as branded borrow-views. The engine implements it; host code
/// never names the concrete type.
///
/// Arguments are exposed as [`HostValue`] borrow-views rather than raw handles, which
/// prevents a host from moving a heap argument into a `'static`
/// [`HostCall::Pending`] future. Both a synchronous and an async
/// return carry only [`OwnedValue`], never a raw handle, so a forged handle is
/// *unrepresentable* in a return. To return a heap value the host pins an argument
/// by position ([`HostContext::pin_arg`] → [`OwnedValue::Pinned`]), which the
/// engine mints and validates; the context exposes no way to obtain a bare
/// `RawValue`.
pub trait HostContext {
    /// The number of call arguments.
    fn arg_count(&self) -> usize;

    /// The argument at `index` as a branded borrow-view, or `None` if out of
    /// range. A host that awaits must pin what it needs
    /// ([`HostContext::pin_arg`]) or snapshot owned data. The stale/forged-handle
    /// guarantee comes from the validated return contract, not the lifetime.
    fn arg(&self, index: usize) -> Option<HostValue<'_>>;

    /// Pins the argument at `index` in the VM registry so a host call can return
    /// it as [`OwnedValue::Pinned`] (a synchronous result, or an async future
    /// after an await) without naming a raw heap handle. Returns `None` if `index`
    /// is out of range. Because the host names an argument by position, never a raw
    /// handle, it cannot pin a forged, stale, or cross-VM value. The engine
    /// consumes the pin when it materializes the observed result.
    fn pin_arg(&mut self, index: usize) -> Option<RegistryRef>;
}

/// How a module's exported name binds into a script's environment.
///
/// Global bindings are fail-closed about collisions with the engine's builtin
/// surface: a [`ModuleBinding::Global`] whose name is already installed (an
/// engine builtin, a surface library global, or an earlier module binding) is
/// a build error, and replacing a builtin requires the explicit
/// [`ModuleBinding::GlobalOverride`] opt-in. An override lands during VM
/// construction, before `Vm::sandbox` freezes the globals, so sandboxed
/// scripts see exactly the overridden surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleBinding {
    /// Installed as a fresh base global. Fails closed at VM build when the
    /// name is already installed — accidental builtin collisions are an
    /// error, never a silent replacement.
    Global,
    /// Replaces an existing base global (such as an engine builtin like
    /// `assert`) — the explicit override opt-in. Fails closed at VM build
    /// when no global of that name exists to override.
    GlobalOverride,
    /// Installed as a member of a named library table.
    Library(Cow<'static, str>),
    /// Installed as a member of a host-only table that never appears in the
    /// script-visible globals. The engine registers the table in the VM's
    /// named registry under the given name, where the host retrieves it with
    /// `Scope::named_get` (for example to wire it as a metatable `__index`).
    /// The table survives `Vm::clear_named_registry`. The module's
    /// declaration must not declare a global for it; type aliases and
    /// classes in the declaration still contribute types to the checker.
    Hidden(Cow<'static, str>),
}

impl ModuleBinding {
    /// Builds a library binding from a static or runtime-owned library name.
    #[must_use]
    pub fn library(name: impl Into<Cow<'static, str>>) -> Self {
        Self::Library(name.into())
    }

    /// Builds a hidden (host-only) binding from a static or runtime-owned
    /// table name.
    #[must_use]
    pub fn hidden(name: impl Into<Cow<'static, str>>) -> Self {
        Self::Hidden(name.into())
    }
}

/// How a native module is exposed to scripts.
///
/// This describes the module table as a whole. Individual entries still use
/// [`ModuleBinding`] to say how their backing functions and constants are
/// installed while the VM is built.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum ModuleExport {
    /// Keep today's behavior: the module contributes globals and library table
    /// members, but `require("<module>")` is not seeded by the native module
    /// itself.
    #[default]
    Globals,
    /// `require("<module>")` returns the native module table and the module
    /// does not install a script-visible global of the same name.
    Require,
    /// The module is require-able and also visible through its global/library
    /// bindings.
    Both,
}

/// An engine-minted high-level host callable payload for
/// [`ModuleBuilder::host_callable`].
///
/// This stable API crate cannot name the engine's scoped/async host-function
/// traits, so the payload is type-erased — but only the engine mints it.
/// Hosts obtain one from the engine's helpers (`scoped_module_host_callable` /
/// `async_module_host_callable` in `ruau-vm`) or skip the payload entirely via
/// the engine's `ModuleBuilderExt` convenience methods, so a hand-built value
/// that would only fail at install time is unrepresentable.
pub struct EngineCallable(Box<dyn Any + Send + Sync>);

impl EngineCallable {
    /// Wraps the engine's type-erased callable payload. Engine-internal:
    /// hosts go through the engine mint helpers instead.
    #[doc(hidden)]
    #[must_use]
    pub fn from_engine(payload: Box<dyn Any + Send + Sync>) -> Self {
        Self(payload)
    }

    /// Unwraps the payload for the engine's install-time downcast.
    /// Engine-internal.
    #[doc(hidden)]
    #[must_use]
    pub fn into_engine(self) -> Box<dyn Any + Send + Sync> {
        self.0
    }
}

impl std::fmt::Debug for EngineCallable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EngineCallable")
    }
}

/// An engine-minted host userdata type descriptor for
/// [`ModuleBuilder::host_type`].
///
/// This stable API crate cannot name the engine's concrete host-type
/// descriptor, so the payload is type-erased — but only the engine mints it.
/// Hosts register host types through the engine's `ModuleBuilderExt::host_type`
/// convenience method, so a hand-built value that would only fail at install
/// time is unrepresentable.
pub struct EngineHostType(Box<dyn Any + Send + Sync>);

impl EngineHostType {
    /// Wraps the engine's type-erased host-type descriptor. Engine-internal:
    /// hosts go through the engine's `ModuleBuilderExt::host_type` instead.
    #[doc(hidden)]
    #[must_use]
    pub fn from_engine(payload: Box<dyn Any + Send + Sync>) -> Self {
        Self(payload)
    }

    /// Unwraps the descriptor for the engine's install-time downcast.
    /// Engine-internal.
    #[doc(hidden)]
    #[must_use]
    pub fn into_engine(self) -> Box<dyn Any + Send + Sync> {
        self.0
    }
}

impl std::fmt::Debug for EngineHostType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EngineHostType")
    }
}

/// The surface a [`NativeModule`] uses to register its functions and constants.
/// Implemented by the engine. Names are borrowed for the duration of the
/// registration call (the engine interns or copies what it keeps), so a module
/// built from runtime-generated strings registers without leaking.
pub trait ModuleBuilder {
    /// Registers a host function under `name` with the given binding.
    fn function(&mut self, name: &str, binding: ModuleBinding, f: Box<dyn HostFunction>);

    /// Registers an engine-owned high-level host callable under `name`.
    ///
    /// Higher-level embedding APIs use this type-erased bridge for scoped or
    /// async host functions whose concrete traits live in the engine crate.
    /// Hosts obtain the payload from engine mint helpers (or call the engine's
    /// `ModuleBuilderExt` convenience methods instead); a payload minted for a
    /// different engine still fails closed during module installation and the
    /// VM refuses execution.
    fn host_callable(&mut self, name: &str, binding: ModuleBinding, f: EngineCallable);

    /// Registers a constant value under `name` with the given binding.
    fn constant(&mut self, name: &str, binding: ModuleBinding, value: ModuleValue);

    /// Registers a value produced by a trusted Luau source chunk.
    ///
    /// The chunk runs once during VM construction, before sandboxing, and must
    /// return exactly one value. The returned value is installed under `name`
    /// with the same collision and native-export rules as other bindings.
    fn source_value(&mut self, name: &str, binding: ModuleBinding, source: &[u8]);

    /// Registers a trusted Luau source value with fixed host-only table inputs.
    ///
    /// The engine resolves every key to a hidden module table before it runs
    /// any registered source value. It passes the tables to `source` as ordered
    /// positional arguments. Keys are copied during registration and are never
    /// exposed to Luau as globals or through a lookup API.
    fn source_value_with(
        &mut self,
        name: &str,
        binding: ModuleBinding,
        source: &[u8],
        private_inputs: &[&str],
    );

    /// Registers a string-keyed constant table under `name` with the given binding.
    fn table(&mut self, name: &str, binding: ModuleBinding, table: ModuleTable) {
        self.constant(name, binding, ModuleValue::Table(table));
    }

    /// Registers an engine-owned host userdata type for this module.
    ///
    /// Higher-level embedding APIs use this type-erased bridge so this stable
    /// API crate does not depend on the engine's concrete host-type
    /// descriptor. Hosts obtain the payload from the engine's
    /// `ModuleBuilderExt::host_type` convenience method; a payload minted for
    /// a different engine still fails closed during surface validation or VM
    /// installation.
    fn host_type(&mut self, ty: EngineHostType);

    /// Registers a trusted Lua support chunk whose single return value is
    /// rooted in the host named registry under `registry_key` at VM build.
    ///
    /// Support chunks run before sandboxing and are hidden from scripts. They
    /// are intended for Lua proxy/helper tables that native host functions can
    /// retrieve by name without enabling script-visible runtime compilation.
    fn support_chunk(&mut self, registry_key: &str, source: &[u8]);
}

/// A native (Rust-backed) module: a typed `.d.luau` surface plus the backing
/// implementation, toggled like any other module. The name and declaration are
/// borrowed from the module itself, so both static and runtime-generated
/// surfaces are representable.
pub trait NativeModule: Send + Sync {
    /// The stable module name.
    fn name(&self) -> &str;

    /// The declaration source this module is checked against.
    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_>;

    /// How this module's table is exposed to scripts.
    fn export(&self) -> ModuleExport {
        ModuleExport::Globals
    }

    /// Installs the module's bindings into `installer`.
    fn install(&self, builder: &mut dyn ModuleBuilder);
}

#[cfg(any())]
mod tests {
    use super::{HeapId, OwnedValue, RegistryRef};

    #[test]
    fn owned_value_type_name_and_display_lua_cover_public_kinds() {
        let pinned = OwnedValue::Pinned(RegistryRef::from_parts(3, 5, HeapId(7)));
        let values = [
            (OwnedValue::Nil, "nil", "nil"),
            (OwnedValue::Boolean(true), "boolean", "true"),
            (OwnedValue::Number(2.0), "number", "2"),
            (OwnedValue::Number(-0.0), "number", "-0"),
            (OwnedValue::Number(1e-7), "number", "1e-07"),
            (OwnedValue::Integer(4), "number", "4"),
            (OwnedValue::Vector([1.0, 2.5, -0.0]), "vector", "1, 2.5, -0"),
            (
                OwnedValue::LightUserdata { handle: 1, tag: 2 },
                "userdata",
                "userdata",
            ),
            (OwnedValue::Bytes(b"hello".to_vec()), "string", "hello"),
            (pinned, "pinned", "pinned"),
        ];

        for (value, type_name, display) in values {
            assert_eq!(value.type_name(), type_name, "{value:?}");
            assert_eq!(value.display_lua(), display, "{value:?}");
        }
    }
}
