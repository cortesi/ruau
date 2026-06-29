//! Serde value bridge: `Serialize` types into scope-borrowed Lua values and
//! back, plus owned [`MarshaledValue`] ⇄ [`serde_json::Value`] conversions.
//!
//! [`to_scoped_value`] runs a serde `Serializer` over a [`Scope`]'s value
//! constructors; [`from_scoped_value`] runs a serde `Deserializer` over a
//! [`ScopedValue`]. Both live inside one scope step, so nothing here escapes
//! the step's brand.
//! [`RetainedTableSchema`] caches VM-local key handles and per-position cleanup
//! state. It writes serializable table-shaped values into an existing Lua table,
//! reusing retained child tables, clearing schema-owned stale string keys and
//! array tails, and defensively scanning mutable tables for ad-hoc string keys.
//!
//! # Encoding
//!
//! The serde data model maps to Lua values as follows:
//!
//! ```text
//! serde                          Lua                  notes
//! -----------------------------  -------------------  --------------------------------
//! bool                           boolean
//! i8..i64, u8..u32               integer              the VM's first-class i64
//! u64                            integer              error above i64::MAX (no
//!                                                     silent precision loss)
//! f32, f64                       number               IEEE-754 double
//! char                           string               one-character string
//! str                            string
//! bytes                          string               Luau strings are byte strings
//! unit, unit struct, None        nil
//! Some(x)                        encoding of x
//! newtype struct                 encoding of inner
//! seq, tuple, tuple struct       table                array-shaped, keys 1..n
//! map, struct                    table                struct fields are string keys
//! unit enum variant              string               "Variant"
//! newtype/tuple/struct variant   table                { Variant = payload }
//! ```
//!
//! Decoding accepts the same shapes back, with two relaxations: an `Integer`
//! *or* an exactly-integral `Number` satisfies an integer type, and an
//! `Integer` satisfies a float type. A string must be UTF-8 wherever serde
//! expects text; `bytes` accepts a string or a buffer. Internally tagged,
//! adjacently tagged, and untagged enum representations are driven by serde
//! itself through the map/struct machinery above and all work; the externally
//! tagged shape in the table is the bridge's native one.
//!
//! ## Divergences and lossy edges
//!
//! - **nil vs absent:** `None` and unit serialize to `nil`, and a `nil` table
//!   field is indistinguishable from an absent one in Lua. A struct field
//!   holding `None` therefore vanishes from the table, and `Some(None)`-style
//!   nesting collapses: it decodes as `None`.
//! - **empty containers:** an empty seq and an empty map both serialize to an
//!   empty table and are indistinguishable on the Lua side.
//!   [`from_scoped_value`] accepts an empty table as either; a
//!   self-describing decode (`deserialize_any`, e.g. `serde_json::Value`)
//!   reads it as an empty *map*, so an empty JSON array round-trips to `{}`.
//! - **integral numbers self-describe as integers:** a `Number` whose value
//!   is exactly integral presents as `i64` to a self-describing decode.
//!   Script-side number literals materialize as `Number` in this revision,
//!   and serde's buffering decodes (internally tagged and untagged enums,
//!   `flatten`, `serde_json::Value`) apply no integral relaxation of their
//!   own — without this, `{ dx = 1 }` could not reach an `i64` field of an
//!   internally tagged variant. Consequence: a JSON float `1.0` round-trips
//!   through `serde_json::Value` as the integer `1`; non-integral floats are
//!   untouched.
//! - **array shape:** sequence elements are written under number keys `1..n`
//!   (the table's array part, matching `#t` and the rest of the embedding
//!   API). Decoding a sequence requires integer keys covering exactly `1..n`
//!   — holes or stray keys fail with a clear error.
//! - **marshal caps:** value trees deeper than the value-marshal default cap,
//!   oversized string/buffer copies, table snapshots, and recursive node counts
//!   fail closed in both directions with marshal-cap errors, mirroring the owned
//!   result marshaler. The caps are fixed at the defaults: a `Scope` does not
//!   carry the per-invocation `Limits` overrides.
//!
//! ## Error paths
//!
//! Conversion errors are prefixed with the path to the failing value, in Lua
//! terms: string keys and struct fields join with `.`, sequence positions are
//! 1-based `[n]`, non-string map keys render as `[key]`. For example:
//!
//! ```text
//! actions[3].kind: unknown variant `move`, expected `stay` or `go`
//! ```
//!
//! The innermost segment is the value that failed: an internally tagged
//! enum's unknown tag fails while decoding the tag field's value, so the path
//! ends in `.kind`; a `deny_unknown_fields` violation fails on the stray key,
//! so the path ends in the stray field's name; `missing field` is raised at
//! the enclosing table. Errors preserve the underlying [`RuntimeError`]
//! failure kind (memory exhaustion stays `Memory`).
//!
//! # Owned JSON conversions
//!
//! [`marshaled_to_json`] and [`json_to_marshaled`] convert the owned
//! [`MarshaledValue`] tree (the `exec_async` result shape)
//! to and from [`serde_json::Value`] without re-entering a scope. These are
//! JSON-document conversions, not the generic serde bridge: `json_to_marshaled`
//! preserves JSON `null` with Ruau's reserved light-userdata sentinel and marks
//! arrays so `[]` remains distinct from `{}`. `marshaled_to_json` recognizes
//! that sentinel and marker, and also accepts ordinary VM-shaped array tables
//! whose keys are exactly the integers `1..n`; an empty unmarked table maps to
//! `{}`. Values that JSON cannot represent — buffers, vectors, non-reserved
//! light userdata, `Opaque` handles, non-UTF-8 strings, and non-finite numbers
//! — fail with a clear, path-prefixed error. The reverse errors on integers
//! above `i64::MAX`.

use std::collections::HashMap;

use ruau_vm_api::{ModuleValue, RuntimeErrorKind};
use serde::{
    Serialize,
    de::{self, DeserializeOwned},
    ser,
};

use crate::{DEFAULT_MAX_VALUE_MARSHAL_DEPTH, KeyHandle, RuntimeError, Scope, ScopedValue, Table};

mod deserializer;
mod marshaled_json;
mod serializer;

use deserializer::{SharedDeserializeBudget, ValueDeserializeBudget, ValueDeserializer};
pub use marshaled_json::{json_to_marshaled, marshaled_to_json};
use serializer::{RetainedValueSerializer, ValueSerializer, new_table};

const JSON_NULL_LIGHTUSERDATA_HANDLE: u32 = 0x4f58_4a4e; // "OXJN"
const JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE: u32 = 0x4f58_4a41; // "OXJA"
const JSON_BRIDGE_LIGHTUSERDATA_TAG: u8 = 0x4f; // "O"

/// Returns the stable module value used for a host-provided `json.null`
/// sentinel.
///
/// Hosts that expose a JSON helper module can install this value as their
/// public `null` constant without depending on the bridge's private
/// lightuserdata handles.
#[must_use]
pub const fn json_null_module_value() -> ModuleValue {
    ModuleValue::LightUserdata {
        handle: JSON_NULL_LIGHTUSERDATA_HANDLE,
        tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
    }
}

/// Serializes `value` into a scope-borrowed Lua value using `scope`'s
/// constructors, per the module-level encoding table.
///
/// # Errors
/// Returns [`RuntimeError`] when a heap allocation fails, an integer exceeds
/// Lua's 64-bit range, the value tree exceeds the marshal depth cap, or the
/// type's `Serialize` implementation reports an error. Messages are prefixed
/// with the path to the failing value.
pub fn to_scoped_value<'s, T: Serialize + ?Sized>(
    scope: &Scope<'s>,
    value: &T,
) -> Result<ScopedValue<'s>, RuntimeError> {
    value
        .serialize(ValueSerializer { scope, depth: 0 })
        .map_err(BridgeError::into_runtime_error)
}

/// Deserializes a `T` from a scope-borrowed Lua value, per the module-level
/// encoding table.
///
/// # Errors
/// Returns [`RuntimeError`] when the value's shape does not match `T` (wrong
/// type, missing field, unknown field or variant, non-array table where a
/// sequence is expected), when an integer is out of range, or when the value
/// tree exceeds a marshal cap. Messages are prefixed with the path to the
/// failing value.
pub fn from_scoped_value<'s, T: DeserializeOwned>(
    scope: &Scope<'s>,
    value: ScopedValue<'s>,
) -> Result<T, RuntimeError> {
    T::deserialize(ValueDeserializer::new(scope, value)).map_err(BridgeError::into_runtime_error)
}

/// Converts a dynamic JSON document into a scope-borrowed Lua value using the
/// lossless JSON policy.
///
/// Unlike [`to_scoped_value`], this preserves JSON `null` with
/// [`Scope::json_null`] and marks arrays with an Ruau-owned protected metatable
/// so `[]` remains distinct from `{}`.
///
/// # Errors
/// Returns [`RuntimeError`] when a heap allocation fails, an integer exceeds
/// Lua's 64-bit range, or the value tree exceeds the marshal depth cap.
pub fn json_to_scoped_value<'s>(
    scope: &Scope<'s>,
    value: &serde_json::Value,
) -> Result<ScopedValue<'s>, RuntimeError> {
    json_to_scoped_value_at(scope, value, 0).map_err(BridgeError::into_runtime_error)
}

/// Converts a scope-borrowed Lua value into a dynamic JSON document using the
/// lossless JSON policy.
///
/// [`Scope::json_null`] encodes as JSON `null`; tables marked by
/// [`json_to_scoped_value`] encode as arrays, including empty arrays. Empty
/// unmarked tables encode as JSON objects.
///
/// # Errors
/// Returns [`RuntimeError`] for values JSON cannot represent, non-array tables
/// carrying the JSON array marker, non-UTF-8 strings or object keys, non-finite
/// numbers, and trees past the marshal depth cap.
pub fn scoped_value_to_json<'s>(
    scope: &Scope<'s>,
    value: ScopedValue<'s>,
) -> Result<serde_json::Value, RuntimeError> {
    let budget = ValueDeserializeBudget::shared_default();
    scoped_value_to_json_at(scope, value, 0, &budget).map_err(BridgeError::into_runtime_error)
}

/// Returns true when `value` is the protected table marker the JSON bridge uses
/// to distinguish arrays from objects.
#[must_use]
pub fn is_json_array_marker(value: ScopedValue<'_>) -> bool {
    is_scoped_json_array_marker(value)
}

/// Host-side cache for writing serde values into a retained Lua table.
///
/// The schema is VM-local: it stores [`KeyHandle`]s, and each handle roots an
/// interned string in the VM that created it. Keep one schema per VM (or per
/// retained observation shape inside that VM), then call [`write`](Self::write)
/// every tick to update an existing table in place.
///
/// The writer uses the same serde-to-Lua encoding as [`to_scoped_value`], but
/// reuses retained tables when a field or sequence slot already holds one.
/// Struct fields skipped by serde are cleared with `nil`, and sequence tails
/// are truncated when a later write is shorter than an earlier one.
#[derive(Clone, Debug)]
pub struct RetainedTableSchema {
    keys: HashMap<String, KeyHandle>,
    nodes: Vec<SchemaNode>,
}

#[derive(Clone, Debug, Default)]
struct SchemaNode {
    children: HashMap<String, SchemaChild>,
    sequence_child: Option<usize>,
    non_string_child: Option<usize>,
    last_keys: Vec<KeyHandle>,
    last_wrote_table: bool,
}

#[derive(Clone, Debug)]
struct SchemaChild {
    handle: KeyHandle,
    node: usize,
}

impl Default for RetainedTableSchema {
    fn default() -> Self {
        Self {
            keys: HashMap::new(),
            nodes: vec![SchemaNode::default()],
        }
    }
}

impl RetainedTableSchema {
    const ROOT_NODE: usize = 0;

    /// Builds an empty retained-table schema.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of interned string keys cached by this schema.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// Serializes `value` into `table`, reusing the table tree where possible.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] for the same encoding failures as
    /// [`to_scoped_value`], or if `value` is not table-shaped at the root.
    pub fn write<'s, T: Serialize + ?Sized>(
        &mut self,
        scope: &Scope<'s>,
        table: Table<'s>,
        value: &T,
    ) -> Result<(), RuntimeError> {
        let encoded = value
            .serialize(RetainedValueSerializer {
                scope,
                schema: self,
                node: Self::ROOT_NODE,
                depth: 0,
                target: Some(table),
            })
            .map_err(BridgeError::into_runtime_error)?;
        if matches!(encoded, ScopedValue::Table(_)) {
            Ok(())
        } else {
            Err(RuntimeError::runtime(
                "retained table writes require a table-shaped serde value",
            ))
        }
    }

    fn key<'s>(&mut self, scope: &Scope<'s>, key: &str) -> Result<KeyHandle, BridgeError> {
        if let Some(handle) = self.keys.get(key) {
            return Ok(handle.clone());
        }
        let handle = scope.intern_key(key).map_err(BridgeError::from)?;
        self.keys.insert(key.to_owned(), handle.clone());
        Ok(handle)
    }

    fn keyed_child<'s>(
        &mut self,
        scope: &Scope<'s>,
        parent: usize,
        key: &str,
    ) -> Result<(KeyHandle, usize), BridgeError> {
        if let Some(child) = self.nodes[parent].children.get(key) {
            return Ok((child.handle.clone(), child.node));
        }
        let handle = self.key(scope, key)?;
        let node = self.nodes.len();
        self.nodes.push(SchemaNode::default());
        self.nodes[parent].children.insert(
            key.to_owned(),
            SchemaChild {
                handle: handle.clone(),
                node,
            },
        );
        Ok((handle, node))
    }

    fn sequence_child_node(&mut self, parent: usize) -> usize {
        if let Some(node) = self.nodes[parent].sequence_child {
            return node;
        }
        let node = self.nodes.len();
        self.nodes.push(SchemaNode::default());
        self.nodes[parent].sequence_child = Some(node);
        node
    }

    fn non_string_child_node(&mut self, parent: usize) -> usize {
        if let Some(node) = self.nodes[parent].non_string_child {
            return node;
        }
        let node = self.nodes.len();
        self.nodes.push(SchemaNode::default());
        self.nodes[parent].non_string_child = Some(node);
        node
    }

    fn should_probe_node(&self, node: usize) -> bool {
        self.nodes[node].last_wrote_table
    }

    fn remember_node_shape(&mut self, node: usize, value: ScopedValue<'_>) {
        self.nodes[node].last_wrote_table = matches!(value, ScopedValue::Table(_));
    }

    fn finish_keyed_node<'s>(
        &mut self,
        scope: &Scope<'s>,
        table: Table<'s>,
        node: usize,
        current: Vec<KeyHandle>,
        clear_non_string: bool,
    ) -> Result<(), BridgeError> {
        let needs_stale_cleanup = {
            let previous = &self.nodes[node].last_keys;
            !previous.is_empty() && previous != &current
        };
        if needs_stale_cleanup {
            let previous = self.nodes[node].last_keys.clone();
            table
                .clear_stale_keyed(scope, previous.iter(), current.iter())
                .map_err(BridgeError::from)?;
        }
        if !table.is_frozen(scope).map_err(BridgeError::from)? {
            table
                .clear_except_keyed(scope, current.iter(), clear_non_string)
                .map_err(BridgeError::from)?;
        }
        self.nodes[node].last_keys = current;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Path-tracking bridge error
// ---------------------------------------------------------------------------

/// One step of the path to a failing value, pushed innermost-first as an
/// error bubbles out of nested containers.
#[derive(Clone, Debug)]
enum Segment {
    /// A string map key or struct field: rendered `.name` (bare when first).
    Field(String),
    /// A 1-based sequence position: rendered `[n]`.
    Index(u64),
    /// A non-string map key: rendered `[key]`.
    Key(String),
}

/// The bridge's internal error: a [`RuntimeError`] plus the path segments
/// collected while unwinding. It implements serde's error traits so it can be
/// the `Serializer`/`Deserializer` error type; the public functions render it
/// back into a path-prefixed [`RuntimeError`].
#[derive(Debug)]
struct BridgeError {
    error: RuntimeError,
    path: Vec<Segment>,
}

impl BridgeError {
    fn new(message: impl std::fmt::Display) -> Self {
        RuntimeError::runtime(message).into()
    }

    /// The fail-closed depth error, mirroring the owned value marshaler's
    /// message and (runtime) failure kind.
    fn depth() -> Self {
        Self::depth_limit(DEFAULT_MAX_VALUE_MARSHAL_DEPTH)
    }

    fn depth_limit(max_depth: usize) -> Self {
        Self::new(format!("value depth exceeds marshal cap {max_depth}"))
    }

    /// Wraps this error with one more (outer) path segment.
    fn at(mut self, segment: Segment) -> Self {
        self.path.push(segment);
        self
    }

    fn rendered_path(&self) -> String {
        let mut out = String::new();
        for segment in self.path.iter().rev() {
            match segment {
                Segment::Field(name) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(name);
                }
                Segment::Index(index) => {
                    out.push('[');
                    out.push_str(&index.to_string());
                    out.push(']');
                }
                Segment::Key(key) => {
                    out.push('[');
                    out.push_str(key);
                    out.push(']');
                }
            }
        }
        out
    }

    /// Renders the path into the message, preserving the failure kind.
    fn into_runtime_error(self) -> RuntimeError {
        if self.path.is_empty() {
            return self.error;
        }
        let message = format!("{}: {}", self.rendered_path(), self.error.message());
        match self.error.kind() {
            RuntimeErrorKind::Memory => RuntimeError::memory(message),
            _ => RuntimeError::runtime(message),
        }
    }
}

impl From<RuntimeError> for BridgeError {
    fn from(error: RuntimeError) -> Self {
        Self {
            error,
            path: Vec::new(),
        }
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(self.error.message())
        } else {
            write!(f, "{}: {}", self.rendered_path(), self.error.message())
        }
    }
}

impl std::error::Error for BridgeError {}

impl ser::Error for BridgeError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Self::new(msg)
    }
}

impl de::Error for BridgeError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Self::new(msg)
    }
}

fn type_error(expected: &str, got: ScopedValue<'_>) -> BridgeError {
    BridgeError::new(format!("expected {expected}, got {}", got.type_name()))
}

/// The `f64` payload of a [`serde_json::Number`] after integer forms are ruled out.
fn json_number_to_f64(number: &serde_json::Number) -> Result<f64, BridgeError> {
    number
        .as_f64()
        .ok_or_else(|| BridgeError::new("JSON number is not representable as f64"))
}

/// The i64 a number denotes exactly, if it is integral and in range.
fn exact_integer(value: f64) -> Option<i64> {
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "fract()==0 and the range guard keep the cast exact"
    )]
    if value.fract() == 0.0 && (-TWO_POW_63..TWO_POW_63).contains(&value) {
        Some(value as i64)
    } else {
        None
    }
}

/// The path segment naming the position of a map entry, derived from its key.
fn key_segment<'s>(scope: &Scope<'s>, key: ScopedValue<'s>) -> Segment {
    match key {
        ScopedValue::String(handle) => match scope.string_bytes(handle) {
            Ok(bytes) => Segment::Field(String::from_utf8_lossy(&bytes).into_owned()),
            Err(_) => Segment::Key("<string>".to_owned()),
        },
        ScopedValue::Integer(value) => Segment::Key(value.to_string()),
        ScopedValue::Number(value) => Segment::Key(value.to_string()),
        ScopedValue::Boolean(value) => Segment::Key(value.to_string()),
        other => Segment::Key(format!("<{}>", other.type_name())),
    }
}

fn json_array_marker<'s>() -> ScopedValue<'s> {
    ScopedValue::LightUserdata {
        handle: JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE,
        tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
    }
}

fn is_scoped_json_null(value: ScopedValue<'_>) -> bool {
    matches!(
        value,
        ScopedValue::LightUserdata {
            handle: JSON_NULL_LIGHTUSERDATA_HANDLE,
            tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
        }
    )
}

fn is_scoped_json_array_marker(value: ScopedValue<'_>) -> bool {
    matches!(
        value,
        ScopedValue::LightUserdata {
            handle: JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE,
            tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
        }
    )
}

fn attach_json_array_marker<'s>(scope: &Scope<'s>, table: Table<'s>) -> Result<(), BridgeError> {
    let metatable = scope.create_table().map_err(BridgeError::from)?;
    metatable
        .set(scope, "__ruau_json_array", json_array_marker())
        .map_err(|error| BridgeError::from(error).at(Segment::Field("__ruau_json_array".into())))?;
    metatable
        .set(scope, "__metatable", "ruau json array")
        .map_err(|error| BridgeError::from(error).at(Segment::Field("__metatable".into())))?;
    metatable.freeze(scope).map_err(BridgeError::from)?;
    table
        .set_metatable(scope, Some(metatable))
        .map_err(BridgeError::from)
}

fn has_json_array_marker<'s>(scope: &Scope<'s>, table: Table<'s>) -> Result<bool, BridgeError> {
    let Some(metatable) = table.metatable(scope).map_err(BridgeError::from)? else {
        return Ok(false);
    };
    let marker: ScopedValue<'_> = metatable
        .get(scope, "__ruau_json_array")
        .map_err(BridgeError::from)?;
    Ok(is_scoped_json_array_marker(marker))
}

fn json_to_scoped_value_at<'s>(
    scope: &Scope<'s>,
    value: &serde_json::Value,
    depth: usize,
) -> Result<ScopedValue<'s>, BridgeError> {
    match value {
        serde_json::Value::Null => Ok(scope.json_null()),
        serde_json::Value::Bool(value) => Ok(ScopedValue::Boolean(*value)),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(ScopedValue::Integer(value))
            } else if number.as_u64().is_some() {
                Err(BridgeError::new("integer out of range for Lua: u64"))
            } else {
                Ok(ScopedValue::Number(json_number_to_f64(number)?))
            }
        }
        serde_json::Value::String(text) => Ok(ScopedValue::String(
            scope
                .create_string(text.as_bytes())
                .map_err(BridgeError::from)?,
        )),
        serde_json::Value::Array(items) => {
            let table = new_table(scope, depth)?;
            for (index, item) in items.iter().enumerate() {
                let value = json_to_scoped_value_at(scope, item, depth + 1)
                    .map_err(|error| error.at(Segment::Index(index as u64 + 1)))?;
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "array indices below 2^53 are exact in f64"
                )]
                table
                    .set(scope, (index + 1) as f64, value)
                    .map_err(|error| {
                        BridgeError::from(error).at(Segment::Index(index as u64 + 1))
                    })?;
            }
            attach_json_array_marker(scope, table)?;
            Ok(ScopedValue::Table(table))
        }
        serde_json::Value::Object(map) => {
            let table = new_table(scope, depth)?;
            for (key, item) in map {
                let value = json_to_scoped_value_at(scope, item, depth + 1)
                    .map_err(|error| error.at(Segment::Field(key.clone())))?;
                table
                    .set(scope, key.as_str(), value)
                    .map_err(|error| BridgeError::from(error).at(Segment::Field(key.clone())))?;
            }
            Ok(ScopedValue::Table(table))
        }
    }
}

fn scoped_value_to_json_at<'s>(
    scope: &Scope<'s>,
    value: ScopedValue<'s>,
    depth: usize,
    budget: &SharedDeserializeBudget,
) -> Result<serde_json::Value, BridgeError> {
    budget.borrow_mut().bump_node()?;
    if is_scoped_json_null(value) {
        return Ok(serde_json::Value::Null);
    }
    match value {
        ScopedValue::Nil => Ok(serde_json::Value::Null),
        ScopedValue::Boolean(value) => Ok(serde_json::Value::Bool(value)),
        ScopedValue::Integer(value) => Ok(serde_json::Value::from(value)),
        // The scoped JSON bridge relaxes integral numbers in both directions
        // (decode already self-describes integral JSON numbers as i64): Luau
        // arithmetic produces doubles, so integral results encode as JSON
        // integers, matching the cjson/dkjson precedent. The owned marshaled
        // path keeps exact float-ness.
        #[allow(clippy::cast_possible_truncation)]
        ScopedValue::Number(value)
            if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 =>
        {
            Ok(serde_json::Value::from(value as i64))
        }
        ScopedValue::Number(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| {
                BridgeError::new(format!(
                    "non-finite number {value} is not representable in JSON"
                ))
            }),
        ScopedValue::String(handle) => {
            let len = scope.string_len(handle).map_err(BridgeError::from)?;
            budget.borrow().charge_string_bytes(len)?;
            match String::from_utf8(scope.string_bytes(handle).map_err(BridgeError::from)?) {
                Ok(text) => Ok(serde_json::Value::String(text)),
                Err(_) => Err(BridgeError::new(
                    "non-UTF-8 string is not representable in JSON",
                )),
            }
        }
        ScopedValue::Table(table) => scoped_table_to_json(scope, table, depth, budget),
        ScopedValue::Vector(_) => Err(BridgeError::new("a vector is not representable in JSON")),
        ScopedValue::Buffer(_) => Err(BridgeError::new("a buffer is not representable in JSON")),
        ScopedValue::LightUserdata { .. } => Err(BridgeError::new(
            "light userdata is not representable in JSON",
        )),
        ScopedValue::Function(_) => {
            Err(BridgeError::new("a function is not representable in JSON"))
        }
        ScopedValue::Userdata(_) => Err(BridgeError::new("userdata is not representable in JSON")),
        ScopedValue::Thread(_) => Err(BridgeError::new("a thread is not representable in JSON")),
    }
}

fn scoped_table_to_json<'s>(
    scope: &Scope<'s>,
    table: Table<'s>,
    depth: usize,
    budget: &SharedDeserializeBudget,
) -> Result<serde_json::Value, BridgeError> {
    let max_depth = budget.borrow().max_depth();
    if depth >= max_depth {
        return Err(BridgeError::depth_limit(max_depth));
    }
    let marked_array = has_json_array_marker(scope, table)?;
    let pair_count = table.pair_count(scope).map_err(BridgeError::from)?;
    budget.borrow_mut().charge_table_entries(pair_count)?;
    let pairs = table.pairs(scope).map_err(BridgeError::from)?;
    match classify_table(pairs) {
        TableShape::Empty if marked_array => Ok(serde_json::Value::Array(Vec::new())),
        TableShape::Empty => Ok(serde_json::Value::Object(serde_json::Map::new())),
        TableShape::Seq(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.into_iter().enumerate() {
                let value = scoped_value_to_json_at(scope, item, depth + 1, budget)
                    .map_err(|error| error.at(Segment::Index(index as u64 + 1)))?;
                out.push(value);
            }
            Ok(serde_json::Value::Array(out))
        }
        TableShape::Map(_) if marked_array => Err(BridgeError::new(
            "JSON array marker requires integer keys 1..n",
        )),
        TableShape::Map(pairs) => {
            let mut map = serde_json::Map::new();
            for (key, value) in pairs {
                let key_deserializer =
                    ValueDeserializer::with_budget(scope, key, depth + 1, budget.clone());
                key_deserializer.charge_node()?;
                let key_text = key_deserializer
                    .utf8_string()
                    .map_err(|_| type_error("string", key))?;
                let value = scoped_value_to_json_at(scope, value, depth + 1, budget)
                    .map_err(|error| error.at(Segment::Field(key_text.clone())))?;
                map.insert(key_text, value);
            }
            Ok(serde_json::Value::Object(map))
        }
    }
}

// ---------------------------------------------------------------------------
// Serializer
// ---------------------------------------------------------------------------

/// Serializer over a [`Scope`]'s value constructors. `depth` is the container
/// nesting level of the value being serialized; tables created at or past the
/// marshal cap fail closed.
fn scoped_string_key<'s>(
    scope: &Scope<'s>,
    value: ScopedValue<'s>,
) -> Result<Option<String>, BridgeError> {
    let ScopedValue::String(handle) = value else {
        return Ok(None);
    };
    let bytes = scope.string_bytes(handle).map_err(BridgeError::from)?;
    Ok(String::from_utf8(bytes).ok())
}

fn sequence_key_index(key: ScopedValue<'_>) -> Option<u64> {
    match key {
        ScopedValue::Integer(value) if value >= 1 => u64::try_from(value).ok(),
        #[expect(
            clippy::cast_possible_truncation,
            reason = "fract()==0 and the lower-bound guard keep positive indices integral"
        )]
        ScopedValue::Number(value) if value.fract() == 0.0 && value >= 1.0 => Some(value as u64),
        _ => None,
    }
}

fn clear_sequence_stale<'s>(
    scope: &Scope<'s>,
    table: Table<'s>,
    keep_len: u64,
) -> Result<(), BridgeError> {
    for (key, _) in table.pairs(scope).map_err(BridgeError::from)? {
        if sequence_key_index(key).is_some_and(|index| index <= keep_len) {
            continue;
        }
        table.set(scope, key, ()).map_err(BridgeError::from)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deserializer
// ---------------------------------------------------------------------------

/// The shape a table presents to serde: array (integer keys exactly `1..n`),
/// map (anything else), or empty (acceptable as either).
enum TableShape<'s> {
    Empty,
    Seq(Vec<ScopedValue<'s>>),
    Map(Vec<(ScopedValue<'s>, ScopedValue<'s>)>),
}

/// The 1-based array index a table key denotes, if it is an integer (native
/// or exactly-integral number) in `1..=len`.
fn array_index(key: ScopedValue<'_>, len: usize) -> Option<usize> {
    let index = match key {
        ScopedValue::Integer(value) => value,
        #[expect(
            clippy::cast_possible_truncation,
            reason = "fract()==0 and the 1..=len bound keep the cast exact"
        )]
        ScopedValue::Number(value) if value.fract() == 0.0 && value >= 1.0 => value as i64,
        _ => return None,
    };
    (index >= 1 && index as u128 <= len as u128).then_some(index as usize)
}

fn classify_table<'s>(pairs: Vec<(ScopedValue<'s>, ScopedValue<'s>)>) -> TableShape<'s> {
    if pairs.is_empty() {
        return TableShape::Empty;
    }
    let mut slots: Vec<Option<ScopedValue<'_>>> = vec![None; pairs.len()];
    for &(key, value) in &pairs {
        let Some(index) = array_index(key, pairs.len()) else {
            return TableShape::Map(pairs);
        };
        if slots[index - 1].replace(value).is_some() {
            // Duplicate logical index (a native-integer and a number key can
            // coexist in this VM revision); not a clean array.
            return TableShape::Map(pairs);
        }
    }
    if slots.iter().any(Option::is_none) {
        // Integer keys did not cover exactly `1..n`; treat as map-shaped.
        return TableShape::Map(pairs);
    }
    TableShape::Seq(slots.into_iter().flatten().collect())
}

#[cfg(any())]
mod tests {
    use proptest::prelude::*;
    use serde::Deserialize;
    use serde_json::{Value, json};

    use super::{
        marshaled_json::{marshaled_json_array_marker_pair, marshaled_json_null},
        *,
    };
    use crate::{
        MarshaledPair, MarshaledValue, ValueMarshalLimits,
        value_marshal::DEFAULT_MAX_VALUE_MARSHAL_NODES,
    };

    /// Runs `f` inside one scope step of a fresh runtime-compilation test VM.
    fn with_scope(f: impl for<'s> FnOnce(&Scope<'s>) -> Result<(), RuntimeError>) {
        let mut vm = crate::Vm::builder()
            .ambient(crate::Ambient::deterministic(0))
            .limits(crate::Limits::unlimited())
            .runtime_capabilities(
                crate::RuntimeCapabilities::default().enable_runtime_compilation(),
            )
            .build()
            .expect("test vm builds");
        vm.step(f).expect("scope step succeeds");
    }

    fn test_limits() -> ValueMarshalLimits {
        ValueMarshalLimits {
            max_depth: DEFAULT_MAX_VALUE_MARSHAL_DEPTH,
            max_nodes: DEFAULT_MAX_VALUE_MARSHAL_NODES,
            max_table_entries: 1024,
            max_string_bytes: 1024,
            max_buffer_bytes: 1024,
        }
    }

    fn from_scoped_value_with_limits<'s, T: DeserializeOwned>(
        scope: &Scope<'s>,
        value: ScopedValue<'s>,
        limits: ValueMarshalLimits,
    ) -> Result<T, RuntimeError> {
        let budget = ValueDeserializeBudget::shared_with_limits(limits);
        T::deserialize(ValueDeserializer::with_budget(scope, value, 0, budget))
            .map_err(BridgeError::into_runtime_error)
    }

    fn scoped_value_to_json_with_limits<'s>(
        scope: &Scope<'s>,
        value: ScopedValue<'s>,
        limits: ValueMarshalLimits,
    ) -> Result<Value, RuntimeError> {
        let budget = ValueDeserializeBudget::shared_with_limits(limits);
        scoped_value_to_json_at(scope, value, 0, &budget).map_err(BridgeError::into_runtime_error)
    }

    /// Round-trips `value` through the bridge and asserts equality.
    fn assert_round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        with_scope(|s| {
            let encoded = to_scoped_value(s, value)?;
            let back: T = from_scoped_value(s, encoded)?;
            assert_eq!(&back, value);
            Ok(())
        });
    }

    /// The error message produced by deserializing `encoded(T)` as `U`.
    fn decode_error<T, U>(value: T) -> String
    where
        T: Serialize,
        U: DeserializeOwned + std::fmt::Debug,
    {
        let mut message = String::new();
        with_scope(|s| {
            let encoded = to_scoped_value(s, &value)?;
            let error = from_scoped_value::<U>(s, encoded).expect_err("decode must fail");
            message = error.message().to_owned();
            Ok(())
        });
        message
    }

    #[test]
    fn scalars_round_trip() {
        assert_round_trip(&true);
        assert_round_trip(&false);
        assert_round_trip(&0_i64);
        assert_round_trip(&i64::MAX);
        assert_round_trip(&i64::MIN);
        assert_round_trip(&42_u32);
        assert_round_trip(&-7_i8);
        assert_round_trip(&1.5_f64);
        assert_round_trip(&-0.25_f32);
        assert_round_trip(&String::new());
        assert_round_trip(&"héllo".to_owned());
        assert_round_trip(&'é');
        assert_round_trip(&());
        assert_round_trip(&Option::<i64>::None);
        assert_round_trip(&Some(7_i64));
    }

    #[test]
    fn unit_and_newtype_structs_are_transparent() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Unit;
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrapped(i64);

        assert_round_trip(&Unit);
        assert_round_trip(&Wrapped(9));
        with_scope(|s| {
            assert!(matches!(to_scoped_value(s, &Unit)?, ScopedValue::Nil));
            assert!(matches!(
                to_scoped_value(s, &Wrapped(9))?,
                ScopedValue::Integer(9)
            ));
            Ok(())
        });
    }

    #[test]
    fn u64_beyond_i64_max_fails_instead_of_losing_precision() {
        with_scope(|s| {
            assert!(matches!(
                to_scoped_value(s, &(i64::MAX as u64))?,
                ScopedValue::Integer(i64::MAX)
            ));
            let error =
                to_scoped_value(s, &(i64::MAX as u64 + 1)).expect_err("u64 overflow must fail");
            assert_eq!(error.message(), "integer out of range for Lua: u64");
            Ok(())
        });
    }

    /// A byte-payload wrapper exercising `serialize_bytes`/`deserialize_byte_buf`
    /// (a plain `Vec<u8>` serializes as a sequence in serde's data model).
    #[derive(PartialEq, Debug)]
    struct Blob(Vec<u8>);

    impl Serialize for Blob {
        fn serialize<S: ser::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_bytes(&self.0)
        }
    }

    impl<'de> Deserialize<'de> for Blob {
        fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct BlobVisitor;
            impl<'de> de::Visitor<'de> for BlobVisitor {
                type Value = Blob;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("bytes")
                }
                fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Blob, E> {
                    Ok(Blob(v))
                }
                fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Blob, E> {
                    Ok(Blob(v.to_vec()))
                }
            }
            deserializer.deserialize_byte_buf(BlobVisitor)
        }
    }

    #[test]
    fn bytes_round_trip_as_lua_strings_including_non_utf8() {
        assert_round_trip(&Blob(vec![0, 159, 146, 150, 255]));
        with_scope(|s| {
            let encoded = to_scoped_value(s, &Blob(vec![1, 2]))?;
            assert!(matches!(encoded, ScopedValue::String(_)));
            Ok(())
        });
    }

    #[test]
    fn bytes_accept_a_buffer_value() {
        with_scope(|s| {
            let buffer = s.create_buffer([7, 8, 9])?;
            let blob: Blob = from_scoped_value(s, ScopedValue::Buffer(buffer))?;
            assert_eq!(blob, Blob(vec![7, 8, 9]));
            Ok(())
        });
    }

    #[test]
    fn from_scoped_value_enforces_string_and_buffer_byte_limits() {
        with_scope(|s| {
            let mut limits = test_limits();
            limits.max_string_bytes = 3;
            let value = to_scoped_value(s, &"abcdef")?;
            let error = from_scoped_value_with_limits::<String>(s, value, limits)
                .expect_err("string over limit");
            assert_eq!(
                error.message(),
                "string is 6 bytes, over the 3-byte marshal cap"
            );

            let mut limits = test_limits();
            limits.max_buffer_bytes = 3;
            let buffer = s.create_buffer([1, 2, 3, 4, 5, 6])?;
            let error =
                from_scoped_value_with_limits::<Blob>(s, ScopedValue::Buffer(buffer), limits)
                    .expect_err("buffer over limit");
            assert_eq!(
                error.message(),
                "buffer is 6 bytes, over the 3-byte marshal cap"
            );
            Ok(())
        });
    }

    #[test]
    fn from_scoped_value_enforces_depth_node_and_table_limits() {
        with_scope(|s| {
            let outer = s.create_table()?;
            let inner = s.create_table()?;
            outer.set(s, "inner", ScopedValue::Table(inner))?;
            let mut limits = test_limits();
            limits.max_depth = 1;
            let error =
                from_scoped_value_with_limits::<Value>(s, ScopedValue::Table(outer), limits)
                    .expect_err("nested table exceeds depth");
            assert!(
                error
                    .message()
                    .contains("value depth exceeds marshal cap 1"),
                "unexpected depth error: {}",
                error.message()
            );

            let values = to_scoped_value(s, &vec![1_i64, 2, 3])?;
            let mut limits = test_limits();
            limits.max_nodes = 2;
            let error = from_scoped_value_with_limits::<Vec<i64>>(s, values, limits)
                .expect_err("node cap rejects third value");
            assert!(
                error
                    .message()
                    .contains("value count exceeds marshal cap 2"),
                "unexpected node error: {}",
                error.message()
            );

            let wide = s.create_table()?;
            wide.set(s, "a", 1_i64)?;
            wide.set(s, "b", 2_i64)?;
            wide.set(s, "c", 3_i64)?;
            let mut limits = test_limits();
            limits.max_table_entries = 2;
            let error = from_scoped_value_with_limits::<Value>(s, ScopedValue::Table(wide), limits)
                .expect_err("wide table exceeds entry cap");
            assert_eq!(error.message(), "table entries exceed marshal cap 2");
            Ok(())
        });
    }

    #[test]
    fn scoped_json_bridge_enforces_deserialize_budget() {
        with_scope(|s| {
            let value = to_scoped_value(s, &"abcdef")?;
            let mut limits = test_limits();
            limits.max_string_bytes = 3;
            let error = scoped_value_to_json_with_limits(s, value, limits)
                .expect_err("JSON bridge rejects over-limit string");
            assert_eq!(
                error.message(),
                "string is 6 bytes, over the 3-byte marshal cap"
            );
            Ok(())
        });
    }

    #[test]
    fn structs_and_nested_maps_round_trip() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Theme {
            name: String,
            colors: std::collections::BTreeMap<String, String>,
            opacity: f64,
            tags: Vec<String>,
        }

        assert_round_trip(&Theme {
            name: "dark".to_owned(),
            colors: [
                ("bg".to_owned(), "#000".to_owned()),
                ("fg".to_owned(), "#fff".to_owned()),
            ]
            .into_iter()
            .collect(),
            opacity: 0.5,
            tags: vec!["a".to_owned(), "b".to_owned()],
        });
        assert_round_trip(&std::collections::BTreeMap::<i64, String>::from([
            (-3, "neg".to_owned()),
            (10, "ten".to_owned()),
        ]));
    }

    #[test]
    fn sequences_are_one_based_array_tables() {
        assert_round_trip(&vec![10_i64, 20, 30]);
        assert_round_trip(&(true, 7_i64, "x".to_owned()));
        assert_round_trip(&Vec::<i64>::new());
        with_scope(|s| {
            let encoded = to_scoped_value(s, &vec![10_i64, 20])?;
            let ScopedValue::Table(table) = encoded else {
                panic!("sequence must encode as a table");
            };
            assert_eq!(table.len(s)?, 2, "elements land in the array part");
            assert_eq!(table.get::<_, i64>(s, 1.0_f64)?, 10);
            Ok(())
        });
    }

    #[test]
    fn retained_table_schema_reuses_tables_and_clears_stale_shape() {
        #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
        #[serde(deny_unknown_fields)]
        struct Entity {
            id: i64,
            x: f64,
        }

        #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
        #[serde(deny_unknown_fields)]
        struct Observation {
            tick: i64,
            label: String,
            entities: Vec<Entity>,
            note: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            transient: Option<i64>,
        }

        with_scope(|s| {
            let mut schema = RetainedTableSchema::new();
            let table = s.create_table()?;
            let first = Observation {
                tick: 1,
                label: "first".to_owned(),
                entities: vec![Entity { id: 10, x: 1.5 }, Entity { id: 20, x: 2.5 }],
                note: Some("warm".to_owned()),
                transient: Some(99),
            };
            schema.write(s, table, &first)?;
            let cached_keys = schema.key_count();
            let decoded: Observation = from_scoped_value(s, ScopedValue::Table(table))?;
            assert_eq!(decoded, first);

            let entities: Table<'_> = table.get(s, "entities")?;
            let first_entity: Table<'_> = entities.get(s, 1.0_f64)?;
            first_entity.set(s, "stale", true)?;

            let second = Observation {
                tick: 2,
                label: "second".to_owned(),
                entities: vec![Entity { id: 30, x: 3.5 }],
                note: None,
                transient: None,
            };
            schema.write(s, table, &second)?;
            assert_eq!(
                schema.key_count(),
                cached_keys,
                "a second write over the same schema reuses interned key handles"
            );

            let decoded: Observation = from_scoped_value(s, ScopedValue::Table(table))?;
            assert_eq!(decoded, second);

            let entities: Table<'_> = table.get(s, "entities")?;
            assert_eq!(entities.len(s)?, 1, "the retained array tail is cleared");
            let first_entity: Table<'_> = entities.get(s, 1.0_f64)?;
            let stale: Option<bool> = first_entity.get(s, "stale")?;
            assert_eq!(stale, None, "nested retained tables clear stale fields");
            let transient: Option<i64> = table.get(s, "transient")?;
            assert_eq!(transient, None, "serde-skipped fields are cleared");
            Ok(())
        });
    }

    #[test]
    fn retained_table_schema_readonly_tables_skip_defensive_stale_scan() {
        #[derive(Clone, Debug, Serialize)]
        struct Observation {
            tick: i64,
            items: Vec<i64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            transient: Option<i64>,
        }

        with_scope(|s| {
            let mut schema = RetainedTableSchema::new();
            let table = s.create_table()?;
            schema.write(
                s,
                table,
                &Observation {
                    tick: 1,
                    items: vec![10, 20, 30],
                    transient: Some(99),
                },
            )?;
            table.freeze_deep(s)?;
            table.set(s, "stale", true)?;

            let items: Table<'_> = table.get(s, "items")?;
            items.set(s, 4.0_f64, 40_i64)?;

            schema.write(
                s,
                table,
                &Observation {
                    tick: 2,
                    items: vec![50],
                    transient: None,
                },
            )?;

            let stale: Option<bool> = table.get(s, "stale")?;
            assert_eq!(
                stale,
                Some(true),
                "frozen retained tables skip the defensive anti-tamper scan"
            );
            let transient: Option<i64> = table.get(s, "transient")?;
            assert_eq!(transient, None, "declared skipped fields still clear");
            let items: Table<'_> = table.get(s, "items")?;
            assert_eq!(items.len(s)?, 1, "sequence tails still clear");
            let tail: Option<i64> = items.get(s, 2.0_f64)?;
            assert_eq!(tail, None);
            Ok(())
        });
    }

    #[test]
    fn retained_table_schema_readonly_variant_switch_clears_previous_variant() {
        #[derive(Clone, Debug, Serialize)]
        enum Action {
            Move { dx: i64 },
            Fire { power: i64 },
        }

        #[derive(Clone, Debug, Serialize)]
        struct Observation {
            action: Action,
        }

        with_scope(|s| {
            let mut schema = RetainedTableSchema::new();
            let table = s.create_table()?;
            schema.write(
                s,
                table,
                &Observation {
                    action: Action::Move { dx: 3 },
                },
            )?;
            table.freeze_deep(s)?;

            schema.write(
                s,
                table,
                &Observation {
                    action: Action::Fire { power: 9 },
                },
            )?;

            let action: Table<'_> = table.get(s, "action")?;
            let stale: Option<Table<'_>> = action.get(s, "Move")?;
            assert!(stale.is_none(), "previous enum variant must be cleared");
            let fire: Table<'_> = action.get(s, "Fire")?;
            assert_eq!(fire.get::<_, i64>(s, "power")?, 9);
            Ok(())
        });
    }

    #[test]
    fn retained_table_schema_readonly_string_map_shrink_clears_owned_keys() {
        #[derive(Clone, Debug, Serialize)]
        struct Observation {
            attrs: std::collections::BTreeMap<String, i64>,
        }

        with_scope(|s| {
            let mut schema = RetainedTableSchema::new();
            let table = s.create_table()?;
            schema.write(
                s,
                table,
                &Observation {
                    attrs: [("gone".to_owned(), 1_i64), ("kept".to_owned(), 2_i64)]
                        .into_iter()
                        .collect(),
                },
            )?;
            table.freeze_deep(s)?;

            schema.write(
                s,
                table,
                &Observation {
                    attrs: [("kept".to_owned(), 3_i64)].into_iter().collect(),
                },
            )?;

            let attrs: Table<'_> = table.get(s, "attrs")?;
            let gone: Option<i64> = attrs.get(s, "gone")?;
            assert_eq!(gone, None, "schema-owned stale map keys clear");
            assert_eq!(attrs.get::<_, i64>(s, "kept")?, 3);
            Ok(())
        });
    }

    #[test]
    fn retained_table_schema_same_key_name_mixed_shapes_keeps_position_retention() {
        #[derive(Clone, Debug, Serialize)]
        struct Inner {
            n: i64,
        }

        #[derive(Clone, Debug, Serialize)]
        struct Nested {
            value: i64,
        }

        #[derive(Clone, Debug, Serialize)]
        struct Observation {
            value: Inner,
            nested: Nested,
        }

        with_scope(|s| {
            let mut schema = RetainedTableSchema::new();
            let table = s.create_table()?;
            schema.write(
                s,
                table,
                &Observation {
                    value: Inner { n: 1 },
                    nested: Nested { value: 10 },
                },
            )?;
            let retained_value: Table<'_> = table.get(s, "value")?;

            schema.write(
                s,
                table,
                &Observation {
                    value: Inner { n: 2 },
                    nested: Nested { value: 20 },
                },
            )?;

            assert_eq!(
                retained_value.get::<_, i64>(s, "n")?,
                2,
                "mixed use of the name 'value' must not rebuild the root value table"
            );
            Ok(())
        });
    }

    #[test]
    fn retained_table_schema_rejects_scalar_roots() {
        with_scope(|s| {
            let mut schema = RetainedTableSchema::new();
            let table = s.create_table()?;
            let error = schema
                .write(s, table, &42_i64)
                .expect_err("retained writes need table-shaped roots");
            assert_eq!(
                error.message(),
                "retained table writes require a table-shaped serde value"
            );
            Ok(())
        });
    }

    #[test]
    fn gapped_integer_keys_classify_as_map_shaped_for_sequences() {
        with_scope(|s| {
            let table = s.create_table()?;
            table.set(s, 1_i64, 10_i64)?;
            table.set(s, 3_i64, 30_i64)?;
            let error = from_scoped_value::<Vec<i64>>(s, ScopedValue::Table(table))
                .expect_err("keys 1 and 3 are not a contiguous 1..n array");
            assert_eq!(
                error.message(),
                "expected an array table (integer keys 1..n), got a map-shaped table"
            );
            Ok(())
        });
    }

    #[test]
    fn a_map_shaped_table_is_rejected_where_a_sequence_is_expected() {
        with_scope(|s| {
            let table = s.create_table()?;
            table.set(s, "x", 1_i64)?;
            let error = from_scoped_value::<Vec<i64>>(s, ScopedValue::Table(table))
                .expect_err("map-shaped table is not a sequence");
            assert_eq!(
                error.message(),
                "expected an array table (integer keys 1..n), got a map-shaped table"
            );
            Ok(())
        });
    }

    #[test]
    fn none_fields_vanish_and_nil_nesting_collapses() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Holder {
            v: Option<i64>,
            w: i64,
        }

        with_scope(|s| {
            // A `None` field is simply absent on the Lua side...
            let encoded = to_scoped_value(s, &Holder { v: None, w: 3 })?;
            let ScopedValue::Table(table) = encoded else {
                panic!("struct must encode as a table");
            };
            assert_eq!(table.pairs(s)?.len(), 1, "the None field left no entry");
            // ...and still decodes as None.
            let back: Holder = from_scoped_value(s, encoded)?;
            assert_eq!(back, Holder { v: None, w: 3 });

            // Some(None) encodes to nil and collapses to None on the way back.
            let nested = to_scoped_value(s, &Some(Option::<i64>::None))?;
            assert!(matches!(nested, ScopedValue::Nil));
            let back: Option<Option<i64>> = from_scoped_value(s, nested)?;
            assert_eq!(back, None);
            Ok(())
        });
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum External {
        Unit,
        New(i64),
        Tup(i64, bool),
        Struct { q: String },
    }

    #[test]
    fn externally_tagged_enums_round_trip_all_variant_kinds() {
        assert_round_trip(&External::Unit);
        assert_round_trip(&External::New(5));
        assert_round_trip(&External::Tup(1, true));
        assert_round_trip(&External::Struct { q: "v".to_owned() });
        with_scope(|s| {
            // A unit variant is a bare string; payload variants are
            // single-pair tables.
            assert!(matches!(
                to_scoped_value(s, &External::Unit)?,
                ScopedValue::String(_)
            ));
            let ScopedValue::Table(table) = to_scoped_value(s, &External::New(5))? else {
                panic!("payload variant must encode as a table");
            };
            assert_eq!(table.get::<_, i64>(s, "New")?, 5);
            Ok(())
        });
    }

    /// The arena-critical shape: Luau singleton unions discriminated on a
    /// `kind` field are serde's internally tagged representation.
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Action {
        Stay,
        Go { dx: i64, dy: i64 },
    }

    #[test]
    fn internally_tagged_enums_round_trip() {
        assert_round_trip(&Action::Stay);
        assert_round_trip(&Action::Go { dx: 1, dy: -2 });
        assert_round_trip(&vec![Action::Stay, Action::Go { dx: 0, dy: 9 }]);
        with_scope(|s| {
            let ScopedValue::Table(table) = to_scoped_value(s, &Action::Go { dx: 1, dy: 2 })?
            else {
                panic!("internally tagged variant must encode as a table");
            };
            assert_eq!(table.get::<_, String>(s, "kind")?, "go");
            assert_eq!(table.get::<_, i64>(s, "dx")?, 1);
            Ok(())
        });
    }

    #[test]
    fn adjacently_tagged_enums_round_trip() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        #[serde(tag = "t", content = "c")]
        enum Adjacent {
            X,
            Y(i64),
            Z { v: bool },
        }

        assert_round_trip(&Adjacent::X);
        assert_round_trip(&Adjacent::Y(3));
        assert_round_trip(&Adjacent::Z { v: true });
    }

    #[test]
    fn untagged_enums_round_trip() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        #[serde(untagged)]
        enum Untagged {
            Num(i64),
            Pair { x: i64, y: i64 },
            Text(String),
        }

        assert_round_trip(&Untagged::Num(4));
        assert_round_trip(&Untagged::Pair { x: 1, y: 2 });
        assert_round_trip(&Untagged::Text("t".to_owned()));
    }

    #[test]
    fn unknown_variant_errors_carry_the_path() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Policy {
            actions: Vec<Action>,
        }

        with_scope(|s| {
            // Externally tagged, at the root: the bare message.
            let bad = to_scoped_value(s, &"move")?;
            let error = from_scoped_value::<External>(s, bad).expect_err("unknown variant");
            assert_eq!(
                error.message(),
                "unknown variant `move`, expected one of `Unit`, `New`, `Tup`, `Struct`"
            );

            // Internally tagged, nested: the path names the failing table.
            let actions = s.create_table()?;
            let first = s.create_table()?;
            first.set(s, "kind", "go")?;
            first.set(s, "dx", 0_i64)?;
            first.set(s, "dy", 0_i64)?;
            actions.set(s, 1.0_f64, first)?;
            let bad = s.create_table()?;
            bad.set(s, "kind", "move")?;
            actions.set(s, 2.0_f64, bad)?;
            let policy = s.create_table()?;
            policy.set(s, "actions", actions)?;

            let error = from_scoped_value::<Policy>(s, ScopedValue::Table(policy))
                .expect_err("unknown kind");
            // The path ends in the tag field: the unknown variant fails while
            // decoding `kind`'s value.
            assert_eq!(
                error.message(),
                "actions[2].kind: unknown variant `move`, expected `stay` or `go`"
            );
            Ok(())
        });
    }

    #[test]
    fn missing_field_errors_carry_the_path() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Outer {
            policy: Inner,
        }
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Inner {
            amount: i64,
        }

        with_scope(|s| {
            let inner = s.create_table()?;
            let outer = s.create_table()?;
            outer.set(s, "policy", inner)?;
            let error = from_scoped_value::<Outer>(s, ScopedValue::Table(outer))
                .expect_err("missing field");
            assert_eq!(error.message(), "policy: missing field `amount`");

            let empty = s.create_table()?;
            let error = from_scoped_value::<Inner>(s, ScopedValue::Table(empty))
                .expect_err("missing field at the root");
            assert_eq!(error.message(), "missing field `amount`");
            Ok(())
        });
    }

    #[test]
    fn deny_unknown_fields_reports_the_stray_field() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        #[serde(deny_unknown_fields)]
        struct Strict {
            a: i64,
            b: bool,
        }

        with_scope(|s| {
            let table = s.create_table()?;
            table.set(s, "a", 1_i64)?;
            table.set(s, "b", true)?;
            table.set(s, "extra", "boom")?;
            let error = from_scoped_value::<Strict>(s, ScopedValue::Table(table))
                .expect_err("unknown field");
            // The stray field's own name is the innermost path segment: the
            // error is raised while decoding the key.
            assert_eq!(
                error.message(),
                "extra: unknown field `extra`, expected `a` or `b`"
            );

            // Without the stray field it decodes.
            let table = s.create_table()?;
            table.set(s, "a", 1_i64)?;
            table.set(s, "b", true)?;
            let strict: Strict = from_scoped_value(s, ScopedValue::Table(table))?;
            assert_eq!(strict, Strict { a: 1, b: true });
            Ok(())
        });
    }

    #[test]
    fn flatten_round_trips() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Outer {
            name: String,
            #[serde(flatten)]
            rest: Inner,
        }
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Inner {
            a: i64,
            b: bool,
        }

        assert_round_trip(&Outer {
            name: "n".to_owned(),
            rest: Inner { a: 3, b: false },
        });
        with_scope(|s| {
            // Flattened fields are plain siblings on the Lua side.
            let encoded = to_scoped_value(
                s,
                &Outer {
                    name: "n".to_owned(),
                    rest: Inner { a: 3, b: false },
                },
            )?;
            let ScopedValue::Table(table) = encoded else {
                panic!("struct must encode as a table");
            };
            assert_eq!(table.get::<_, i64>(s, "a")?, 3);
            Ok(())
        });
    }

    #[test]
    fn wrong_type_errors_carry_the_path() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Holder {
            items: Vec<i64>,
        }

        assert_eq!(
            decode_error::<_, (i64, i64)>((1_i64, "x")),
            "[2]: expected integer, got string"
        );
        let message = decode_error::<_, Holder>(json!({ "items": [1, true, 3] }));
        assert_eq!(message, "items[2]: expected integer, got boolean");
    }

    #[test]
    fn integers_accept_exactly_integral_numbers_only() {
        with_scope(|s| {
            let n: i64 = from_scoped_value(s, ScopedValue::Number(3.0))?;
            assert_eq!(n, 3);
            let n: u8 = from_scoped_value(s, ScopedValue::Number(255.0))?;
            assert_eq!(n, 255);

            let error = from_scoped_value::<i64>(s, ScopedValue::Number(3.5))
                .expect_err("non-integral number");
            assert_eq!(
                error.message(),
                "expected integer, got non-integral number 3.5"
            );

            let error = from_scoped_value::<i64>(s, ScopedValue::Number(1e19))
                .expect_err("out-of-range number");
            assert!(
                error
                    .message()
                    .contains("out of range for a 64-bit integer"),
                "unexpected message: {}",
                error.message()
            );

            let error = from_scoped_value::<u8>(s, ScopedValue::Integer(300))
                .expect_err("out-of-range integer");
            assert_eq!(error.message(), "integer out of range for u8");

            let error = from_scoped_value::<u64>(s, ScopedValue::Integer(-1))
                .expect_err("negative integer for u64");
            assert_eq!(error.message(), "integer out of range for u64");

            // Floats accept integers (the documented relaxation).
            let f: f64 = from_scoped_value(s, ScopedValue::Integer(2))?;
            assert!((f - 2.0).abs() < f64::EPSILON);
            Ok(())
        });
    }

    #[test]
    fn serialization_depth_cap_fails_closed() {
        let mut value = json!(1);
        for _ in 0..1000 {
            value = json!([value]);
        }
        with_scope(|s| {
            let error = to_scoped_value(s, &value).expect_err("depth cap");
            assert!(
                error
                    .message()
                    .contains("value depth exceeds marshal cap 64"),
                "unexpected message: {}",
                error.message()
            );
            Ok(())
        });
    }

    #[test]
    fn deserialization_depth_cap_fails_closed() {
        with_scope(|s| {
            let mut table = s.create_table()?;
            for _ in 0..1000 {
                let outer = s.create_table()?;
                outer.set(s, 1.0_f64, table)?;
                table = outer;
            }
            let error =
                from_scoped_value::<Value>(s, ScopedValue::Table(table)).expect_err("depth cap");
            assert!(
                error
                    .message()
                    .contains("value depth exceeds marshal cap 64"),
                "unexpected message: {}",
                error.message()
            );
            Ok(())
        });
    }

    #[test]
    fn json_value_matrix_round_trips() {
        for value in [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(1),
            json!(-1),
            json!(i64::MAX),
            json!(i64::MIN),
            json!(1.5),
            json!(-0.25),
            json!(1.0), // integral float: comes back as the integer 1
            json!(""),
            json!("héllo"),
            json!([1, "two", 3.5, true]),
            json!({"a": 1, "b": [true, "x"], "c": {"d": null}}),
            json!({}),
            json!([[1], [2, 3]]),
        ] {
            with_scope(|s| {
                let encoded = to_scoped_value(s, &value)?;
                let back: Value = from_scoped_value(s, encoded)?;
                assert_eq!(back, normalize(value.clone()), "round trip of {value}");
                Ok(())
            });
        }
    }

    #[test]
    fn empty_json_array_round_trips_to_an_empty_object() {
        // The documented divergence: [] and {} are one Lua value.
        with_scope(|s| {
            let encoded = to_scoped_value(s, &json!([]))?;
            let back: Value = from_scoped_value(s, encoded)?;
            assert_eq!(back, json!({}));
            Ok(())
        });
    }

    #[test]
    fn json_nulls_inside_arrays_pin_the_hole_behavior() {
        with_scope(|s| {
            // A trailing null leaves a shorter array.
            let encoded = to_scoped_value(s, &json!([1, null]))?;
            let back: Value = from_scoped_value(s, encoded)?;
            assert_eq!(back, json!([1]));

            // An interior null leaves a non-array table whose number key
            // cannot become a JSON object key.
            let encoded = to_scoped_value(s, &json!([null, 1]))?;
            let error = from_scoped_value::<Value>(s, encoded).expect_err("hole at index 1");
            assert!(
                error.message().contains("expected string, got number"),
                "unexpected message: {}",
                error.message()
            );
            Ok(())
        });
    }

    #[test]
    fn json_fidelity_scoped_round_trips_null_and_empty_containers() {
        let value = json!({
            "delete": null,
            "empty": [],
            "object": {},
            "nested": [null, [], {}],
        });

        with_scope(|s| {
            let encoded = json_to_scoped_value(s, &value)?;
            let back = scoped_value_to_json(s, encoded)?;
            assert_eq!(back, value);

            let ScopedValue::Table(root) = encoded else {
                panic!("root JSON object should encode as a table");
            };
            let delete: ScopedValue<'_> = root.get(s, "delete")?;
            assert!(is_scoped_json_null(delete));

            let empty: Table<'_> = root.get(s, "empty")?;
            assert!(has_json_array_marker(s, empty).map_err(BridgeError::into_runtime_error)?);

            let object: Table<'_> = root.get(s, "object")?;
            assert!(!has_json_array_marker(s, object).map_err(BridgeError::into_runtime_error)?);
            Ok(())
        });
    }

    #[test]
    fn json_fidelity_owned_marshal_preserves_scoped_array_markers() {
        let value = json!({
            "delete": null,
            "empty": [],
            "object": {},
            "nested": [null, [], {}],
        });

        with_scope(|s| {
            let encoded = json_to_scoped_value(s, &value)?;
            let marshaled = s.marshal(encoded)?;
            assert_eq!(marshaled_to_json(&marshaled)?, value);
            Ok(())
        });
    }

    #[test]
    fn json_fidelity_empty_unmarked_table_encodes_as_object() {
        with_scope(|s| {
            let table = s.create_table()?;
            let back = scoped_value_to_json(s, ScopedValue::Table(table))?;
            assert_eq!(back, json!({}));
            Ok(())
        });
    }

    #[test]
    fn scope_marshal_preserves_json_array_markers() {
        let mut vm = crate::Vm::builder()
            .ambient(crate::Ambient::deterministic(0))
            .limits(crate::Limits::unlimited())
            .runtime_capabilities(
                crate::RuntimeCapabilities::default().enable_runtime_compilation(),
            )
            .build()
            .expect("test vm builds");
        let marshaled = vm
            .step(|s| {
                let encoded = json_to_scoped_value(
                    s,
                    &json!({
                        "empty": [],
                        "nested": [[], {"items": []}],
                    }),
                )?;
                s.marshal(encoded)
            })
            .expect("JSON value marshals");

        assert_eq!(
            marshaled_to_json(&marshaled).expect("marshaled JSON bridge value decodes"),
            json!({
                "empty": [],
                "nested": [[], {"items": []}],
            })
        );
    }

    #[test]
    fn scope_json_null_has_stable_marshaled_identity() {
        let mut vm = crate::Vm::builder()
            .ambient(crate::Ambient::deterministic(0))
            .limits(crate::Limits::unlimited())
            .runtime_capabilities(
                crate::RuntimeCapabilities::default().enable_runtime_compilation(),
            )
            .build()
            .expect("test vm builds");
        let first = vm
            .step(|s| s.marshal(s.json_null()))
            .expect("first sentinel marshals");
        let second = vm
            .step(|s| s.marshal(s.json_null()))
            .expect("second sentinel marshals");

        assert_eq!(first, marshaled_json_null());
        assert_eq!(first, second);
        assert_eq!(
            marshaled_to_json(&first).expect("sentinel to JSON"),
            json!(null)
        );
    }

    #[test]
    fn json_array_marker_is_protected_from_scripts() {
        let mut vm = crate::Vm::builder()
            .ambient(crate::Ambient::deterministic(0))
            .limits(crate::Limits::unlimited())
            .runtime_capabilities(
                crate::RuntimeCapabilities::default().enable_runtime_compilation(),
            )
            .build()
            .expect("test vm builds");
        let chunk = ruau_bytecode::compile_source(
            "return function(t)
                local mt = getmetatable(t)
                local ok = pcall(setmetatable, t, {})
                return mt, ok
            end",
            &ruau_bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
        let module = vm.load(&chunk).expect("load");

        vm.step(|s| {
            let main = s.module_function(&module);
            let inspect: crate::Function<'_> = s.call(main, ())?;
            let ScopedValue::Table(array) = json_to_scoped_value(s, &json!([]))? else {
                panic!("JSON array should encode as a table");
            };
            let (metatable, ok): (String, bool) = s.call(inspect, (ScopedValue::Table(array),))?;
            assert_eq!(metatable, "ruau json array");
            assert!(!ok, "protected metatable rejects script-side replacement");
            assert!(has_json_array_marker(s, array).map_err(BridgeError::into_runtime_error)?);
            Ok(())
        })
        .expect("scope step");
    }

    #[test]
    fn json_integer_and_float_boundaries() {
        with_scope(|s| {
            // i64::MAX survives exactly through the Integer representation.
            let encoded = to_scoped_value(s, &json!(i64::MAX))?;
            assert!(matches!(encoded, ScopedValue::Integer(i64::MAX)));
            let back: Value = from_scoped_value(s, encoded)?;
            assert_eq!(back.as_i64(), Some(i64::MAX));

            // A u64 beyond i64::MAX fails instead of rounding.
            let error = to_scoped_value(s, &json!(u64::MAX)).expect_err("u64 overflow must fail");
            assert_eq!(error.message(), "integer out of range for Lua: u64");

            // On the Lua side floats stay floats and integers stay integers...
            let float = to_scoped_value(s, &json!(2.0))?;
            assert!(matches!(float, ScopedValue::Number(_)));
            let int = to_scoped_value(s, &json!(2))?;
            assert!(matches!(int, ScopedValue::Integer(2)));
            // ...but a self-describing decode reads an exactly-integral
            // number as an integer (the documented normalization), while a
            // fractional number stays a float.
            let back: Value = from_scoped_value(s, ScopedValue::Number(2.0))?;
            assert_eq!(back, json!(2));
            let back: Value = from_scoped_value(s, ScopedValue::Number(2.5))?;
            assert_eq!(back, json!(2.5));
            Ok(())
        });
    }

    #[test]
    fn marshaled_to_json_maps_scalars_tables_and_arrays() {
        assert_eq!(
            marshaled_to_json(&MarshaledValue::Nil).expect("nil"),
            json!(null)
        );
        assert_eq!(
            marshaled_to_json(&MarshaledValue::Boolean(true)).expect("bool"),
            json!(true)
        );
        assert_eq!(
            marshaled_to_json(&MarshaledValue::Integer(7)).expect("integer"),
            json!(7)
        );
        assert_eq!(
            marshaled_to_json(&MarshaledValue::Number(1.5)).expect("number"),
            json!(1.5)
        );
        assert_eq!(
            marshaled_to_json(&MarshaledValue::String(b"hi".to_vec())).expect("string"),
            json!("hi")
        );

        // Number keys 1..n (the VM's array-part shape) become a JSON array;
        // native-integer keys count too.
        let array = MarshaledValue::Table(vec![
            MarshaledPair {
                key: MarshaledValue::Number(1.0),
                value: MarshaledValue::Integer(10),
            },
            MarshaledPair {
                key: MarshaledValue::Integer(2),
                value: MarshaledValue::String(b"x".to_vec()),
            },
        ]);
        assert_eq!(marshaled_to_json(&array).expect("array"), json!([10, "x"]));

        let object = MarshaledValue::Table(vec![MarshaledPair {
            key: MarshaledValue::String(b"k".to_vec()),
            value: MarshaledValue::Boolean(false),
        }]);
        assert_eq!(
            marshaled_to_json(&object).expect("object"),
            json!({"k": false})
        );

        assert_eq!(
            marshaled_to_json(&MarshaledValue::Table(Vec::new())).expect("empty"),
            json!({})
        );
    }

    #[test]
    fn marshaled_to_json_rejects_marked_arrays_with_gapped_keys() {
        let table = MarshaledValue::Table(vec![
            marshaled_json_array_marker_pair(),
            MarshaledPair {
                key: MarshaledValue::Integer(1),
                value: MarshaledValue::Integer(10),
            },
            MarshaledPair {
                key: MarshaledValue::Integer(3),
                value: MarshaledValue::Integer(30),
            },
        ]);
        let error = marshaled_to_json(&table).expect_err("gapped marked array");
        assert_eq!(
            error.message(),
            "JSON array marker requires integer keys 1..n"
        );
    }

    #[test]
    fn marshaled_to_json_rejects_non_representable_values_with_paths() {
        let error =
            marshaled_to_json(&MarshaledValue::Opaque("function")).expect_err("opaque is not JSON");
        assert_eq!(
            error.message(),
            "an opaque function value is not representable in JSON"
        );

        let error = marshaled_to_json(&MarshaledValue::Vector([1.0, 2.0, 3.0]))
            .expect_err("vector is not JSON");
        assert_eq!(error.message(), "a vector is not representable in JSON");

        let error =
            marshaled_to_json(&MarshaledValue::Buffer(vec![1])).expect_err("buffer is not JSON");
        assert_eq!(error.message(), "a buffer is not representable in JSON");

        let error = marshaled_to_json(&MarshaledValue::String(vec![0xff]))
            .expect_err("non-UTF-8 is not JSON");
        assert_eq!(
            error.message(),
            "non-UTF-8 string is not representable in JSON"
        );

        let error =
            marshaled_to_json(&MarshaledValue::Number(f64::NAN)).expect_err("NaN is not JSON");
        assert_eq!(
            error.message(),
            "non-finite number NaN is not representable in JSON"
        );

        // Nested failures carry the path.
        let nested = MarshaledValue::Table(vec![MarshaledPair {
            key: MarshaledValue::String(b"a".to_vec()),
            value: MarshaledValue::Table(vec![MarshaledPair {
                key: MarshaledValue::Number(1.0),
                value: MarshaledValue::Opaque("function"),
            }]),
        }]);
        let error = marshaled_to_json(&nested).expect_err("nested opaque");
        assert_eq!(
            error.message(),
            "a[1]: an opaque function value is not representable in JSON"
        );

        let bad_key = MarshaledValue::Table(vec![MarshaledPair {
            key: MarshaledValue::Boolean(true),
            value: MarshaledValue::Nil,
        }]);
        let error = marshaled_to_json(&bad_key).expect_err("boolean key");
        assert_eq!(
            error.message(),
            "table key of type boolean is not representable as a JSON object key"
        );
    }

    #[test]
    fn json_to_marshaled_round_trips_through_marshaled_to_json() {
        for value in [
            json!(null),
            json!(true),
            json!(-3),
            json!(0.5),
            json!("text"),
            json!([]),
            json!([1, "two", false]),
            json!({"a": [1, 2], "b": {"c": "d"}}),
            json!({}),
            json!({
                "delete": null,
                "empty": [],
                "object": {},
                "nested": [null, [], {}],
            }),
        ] {
            let marshaled = json_to_marshaled(&value).expect("to marshaled");
            let back = marshaled_to_json(&marshaled).expect("back to json");
            assert_eq!(back, value, "round trip of {value}");
        }

        // Array keys use the VM marshaler's number-key shape, with an Ruau
        // marker pair to distinguish empty arrays from empty objects.
        let marshaled = json_to_marshaled(&json!([7])).expect("array");
        assert_eq!(
            marshaled,
            MarshaledValue::Table(vec![
                marshaled_json_array_marker_pair(),
                MarshaledPair {
                    key: MarshaledValue::Number(1.0),
                    value: MarshaledValue::Integer(7),
                }
            ])
        );

        let error = json_to_marshaled(&json!(u64::MAX)).expect_err("u64 overflow");
        assert_eq!(error.message(), "integer out of range for Lua: u64");
        let error =
            json_to_marshaled(&json!({ "big": [u64::MAX] })).expect_err("nested u64 overflow");
        assert_eq!(error.message(), "big[1]: integer out of range for Lua: u64");
    }

    #[test]
    fn marshaled_to_json_recognizes_reserved_json_sentinels() {
        assert_eq!(
            marshaled_to_json(&marshaled_json_null()).expect("sentinel"),
            json!(null)
        );
        assert_eq!(
            marshaled_to_json(&MarshaledValue::Table(vec![
                marshaled_json_array_marker_pair()
            ]))
            .expect("marked empty array"),
            json!([])
        );
        assert_eq!(
            marshaled_to_json(&json_to_marshaled(&json!([])).expect("empty array"))
                .expect("encoded empty array"),
            json!([])
        );
    }

    #[test]
    fn script_produced_tables_decode_through_the_bridge() {
        let mut vm = crate::Vm::builder()
            .ambient(crate::Ambient::deterministic(0))
            .limits(crate::Limits::unlimited())
            .runtime_capabilities(
                crate::RuntimeCapabilities::default().enable_runtime_compilation(),
            )
            .build()
            .expect("test vm builds");
        let chunk = ruau_bytecode::compile_source(
            "return { kind = 'go', dx = 1, dy = -2 }, {10, 20, 30}",
            &ruau_bytecode::CompileOptions::for_vm_execution(),
        )
        .expect("compile");
        let module = vm.load(&chunk).expect("load");
        vm.step(|s| {
            let main = s.module_function(&module);
            let (action, numbers): (ScopedValue<'_>, ScopedValue<'_>) = s.call(main, ())?;
            let action: Action = from_scoped_value(s, action)?;
            assert_eq!(action, Action::Go { dx: 1, dy: -2 });
            let numbers: Vec<i64> = from_scoped_value(s, numbers)?;
            assert_eq!(numbers, vec![10, 20, 30]);
            Ok(())
        })
        .expect("step");
    }

    /// A recursive JSON value model for the round-trip property: no nulls
    /// inside containers (nil == absent would make them vanish) and no
    /// non-finite floats.
    fn json_strategy() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            any::<f64>()
                .prop_filter("finite", |f| f.is_finite())
                .prop_map(Value::from),
            "[a-zA-Z0-9_]{0,8}".prop_map(Value::from),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::btree_map("[a-z]{0,4}", inner, 0..4)
                    .prop_map(|map| Value::Object(map.into_iter().collect())),
            ]
        })
    }

    /// What the bridge round trip preserves: empty arrays come back as empty
    /// objects (one Lua value), exactly-integral floats come back as
    /// integers (self-describing decode), and null object fields vanish
    /// (nil == absent).
    fn normalize(value: Value) -> Value {
        match value {
            Value::Number(number) => match number.as_f64().and_then(exact_integer) {
                Some(int) if number.as_i64().is_none() && number.as_u64().is_none() => {
                    Value::from(int)
                }
                _ => Value::Number(number),
            },
            Value::Array(items) if items.is_empty() => Value::Object(serde_json::Map::new()),
            Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .filter(|(_, value)| !value.is_null())
                    .map(|(key, value)| (key, normalize(value)))
                    .collect(),
            ),
            other => other,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn json_values_round_trip_through_the_bridge(value in json_strategy()) {
            let mut vm = crate::Vm::builder().ambient(crate::Ambient::deterministic(0)).limits(crate::Limits::unlimited()).runtime_capabilities(crate::RuntimeCapabilities::default().enable_runtime_compilation()).build().expect("test vm builds");
            vm.step(|s| {
                let encoded = to_scoped_value(s, &value)?;
                let back: Value = from_scoped_value(s, encoded)?;
                assert_eq!(back, normalize(value.clone()));
                Ok(())
            })
            .expect("scope step succeeds");
        }

        #[test]
        fn json_values_round_trip_through_marshaled(value in json_strategy()) {
            let marshaled = json_to_marshaled(&value).expect("to marshaled");
            let back = marshaled_to_json(&marshaled).expect("back to json");
            assert_eq!(back, value);
        }
    }
}
