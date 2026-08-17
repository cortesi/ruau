//! Owned value marshaling for values leaving a VM entry point.
//!
//! Host returns use `OwnedValue` because they may need to materialize registry
//! pins back into the VM. Entry results flow the other way: they must be plain
//! owned data, with no raw or registry handles escaping after the VM borrow ends.

use std::str;

use crate::{
    api::{RawGc, RawValue, marker},
    heap::Heap,
    limits::EffectiveLimits,
    scope::{JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE, JSON_BRIDGE_LIGHTUSERDATA_TAG},
    serde::JSON_ARRAY_MARKER_KEY,
    table::LuaTable,
    vmutils,
};

/// Default maximum nesting depth for one marshaled value tree.
pub const DEFAULT_MAX_VALUE_MARSHAL_DEPTH: usize = 64;
/// Default maximum number of values copied into one marshaled result tree.
pub const DEFAULT_MAX_VALUE_MARSHAL_NODES: usize = 1 << 20;

/// One plain owned value copied out of the VM.
///
/// Serializable: the serde representation is the durable-backend codec (a
/// Durable Object storage row, a queue payload). `Opaque` round-trips its
/// kind name; deserialization re-canonicalizes the known kinds and falls
/// back to `"opaque"` for anything else, so the variant stays `&'static`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum ValueSnapshot {
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
    /// String bytes copied out of the heap.
    String(Vec<u8>),
    /// Buffer bytes copied out of the heap.
    Buffer(Vec<u8>),
    /// Raw table entries, snapshot in Luau iteration order.
    Table(Vec<MarshaledPair>),
    /// Heap-backed value that is intentionally not copied by this MVP.
    Opaque(&'static str),
}

impl ValueSnapshot {
    /// Luau's ordinary type name for this marshaled value.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Nil => "nil",
            Self::Boolean(_) => "boolean",
            Self::Number(_) | Self::Integer(_) => "number",
            Self::Vector(_) => "vector",
            Self::LightUserdata { .. } => "userdata",
            Self::String(_) => "string",
            Self::Buffer(_) => "buffer",
            Self::Table(_) => "table",
            Self::Opaque(kind) => kind,
        }
    }

    /// Conservative display text for this marshaled value.
    ///
    /// Strings return their bytes lossily decoded as UTF-8. Scalar values use
    /// Luau's scalar spelling. Table and opaque values return their type name;
    /// this helper cannot run `tostring` or inspect VM object identity.
    #[must_use]
    pub fn display_lua(&self) -> String {
        match self {
            Self::Nil => "nil".to_owned(),
            Self::Boolean(true) => "true".to_owned(),
            Self::Boolean(false) => "false".to_owned(),
            Self::Number(value) => vmutils::number_to_string(*value),
            Self::Integer(value) => value.to_string(),
            Self::Vector(value) => value
                .iter()
                .map(|component| vmutils::number_to_string(f64::from(*component)))
                .collect::<Vec<_>>()
                .join(", "),
            Self::LightUserdata { .. } => "userdata".to_owned(),
            Self::String(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Self::Buffer(_) | Self::Table(_) | Self::Opaque(_) => self.type_name().to_owned(),
        }
    }

    /// Borrows this value as table entries.
    ///
    /// # Errors
    /// Returns [`ValueAccessError::ExpectedTable`] for a non-table value.
    pub fn as_table(&self) -> Result<&[MarshaledPair], ValueAccessError> {
        match self {
            Self::Table(pairs) => Ok(pairs),
            value => Err(ValueAccessError::ExpectedTable {
                actual: value.type_name(),
            }),
        }
    }

    /// Borrows the first string-keyed table field in iteration order.
    ///
    /// Non-string keys do not match a field name. A missing field returns
    /// `Ok(None)`.
    ///
    /// # Errors
    /// Returns [`ValueAccessError::ExpectedTable`] for a non-table value.
    pub fn table_field(&self, field: &str) -> Result<Option<&Self>, ValueAccessError> {
        Ok(self.as_table()?.iter().find_map(|pair| {
            matches!(&pair.key, Self::String(key) if key == field.as_bytes()).then_some(&pair.value)
        }))
    }

    /// Borrows one string table field as strict UTF-8.
    ///
    /// A missing field returns `Ok(None)`.
    ///
    /// # Errors
    /// Returns an error for a non-table receiver, a present non-string field,
    /// or invalid UTF-8 string bytes.
    pub fn str_field<'a>(&'a self, field: &str) -> Result<Option<&'a str>, ValueAccessError> {
        let Some(value) = self.table_field(field)? else {
            return Ok(None);
        };
        let Self::String(bytes) = value else {
            return Err(ValueAccessError::ExpectedString {
                field: field.to_owned(),
                actual: value.type_name(),
            });
        };
        str::from_utf8(bytes)
            .map(Some)
            .map_err(|_| ValueAccessError::InvalidUtf8 {
                field: field.to_owned(),
            })
    }
}

/// Failure to access one typed field in an owned value snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ValueAccessError {
    /// The receiver is not a table.
    ExpectedTable {
        /// Luau type of the receiver.
        actual: &'static str,
    },
    /// A present field is not a string.
    ExpectedString {
        /// Requested field name.
        field: String,
        /// Luau type of the field value.
        actual: &'static str,
    },
    /// A present string field is not valid UTF-8.
    InvalidUtf8 {
        /// Requested field name.
        field: String,
    },
}

impl std::fmt::Display for ValueAccessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedTable { actual } => write!(formatter, "expected table, got {actual}"),
            Self::ExpectedString { field, actual } => {
                write!(formatter, "field `{field}` must be a string, got {actual}")
            }
            Self::InvalidUtf8 { field } => {
                write!(formatter, "field `{field}` is not valid UTF-8")
            }
        }
    }
}

impl std::error::Error for ValueAccessError {}

/// One owned table key/value pair.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct MarshaledPair {
    /// Copied table key.
    pub key: ValueSnapshot,
    /// Copied table value.
    pub value: ValueSnapshot,
}

impl<'de> serde::Deserialize<'de> for ValueSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Owned mirror of [`ValueSnapshot`] for deserialization; `Opaque`
        /// carries an owned string re-canonicalized to the static kind set.
        #[derive(serde::Deserialize)]
        enum Wire {
            Nil,
            Boolean(bool),
            Number(f64),
            Integer(i64),
            Vector([f32; 3]),
            LightUserdata { handle: u32, tag: u8 },
            String(Vec<u8>),
            Buffer(Vec<u8>),
            Table(Vec<MarshaledPair>),
            Opaque(String),
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Nil => Self::Nil,
            Wire::Boolean(b) => Self::Boolean(b),
            Wire::Number(n) => Self::Number(n),
            Wire::Integer(i) => Self::Integer(i),
            Wire::Vector(v) => Self::Vector(v),
            Wire::LightUserdata { handle, tag } => Self::LightUserdata { handle, tag },
            Wire::String(bytes) => Self::String(bytes),
            Wire::Buffer(bytes) => Self::Buffer(bytes),
            Wire::Table(pairs) => Self::Table(pairs),
            Wire::Opaque(kind) => Self::Opaque(match kind.as_str() {
                "function" => "function",
                "userdata" => "userdata",
                "thread" => "thread",
                _ => "opaque",
            }),
        })
    }
}

/// Limits used while copying values out of a VM.
#[derive(Clone, Copy, Debug)]
pub struct ValueMarshalLimits {
    /// Maximum recursive value-tree depth.
    pub max_depth: usize,
    /// Maximum scalar/table/key/value nodes copied across the whole tree.
    pub max_nodes: usize,
    /// Maximum table entries copied across the whole tree.
    pub max_table_entries: usize,
    /// Maximum bytes copied from one string.
    pub max_string_bytes: usize,
    /// Maximum bytes copied from one buffer.
    pub max_buffer_bytes: usize,
}

impl From<EffectiveLimits> for ValueMarshalLimits {
    fn from(limits: EffectiveLimits) -> Self {
        Self {
            max_depth: limits.max_value_marshal_depth,
            max_nodes: limits.max_value_marshal_nodes,
            max_table_entries: limits.max_table_elements,
            max_string_bytes: limits.max_string_bytes,
            max_buffer_bytes: limits.max_buffer_bytes,
        }
    }
}

/// Path-aware error raised while marshaling values out of the VM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValueMarshalError {
    path: String,
    message: String,
}

impl ValueMarshalError {
    fn new(path: &str, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValueMarshalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for ValueMarshalError {}

/// Visitor that copies VM values into a plain owned tree under explicit limits.
pub struct ValueVisitor<'h> {
    heap: &'h Heap,
    payloads: &'h crate::host_type::HostPayloadStore,
    limits: ValueMarshalLimits,
    path: Vec<String>,
    active_tables: Vec<RawGc<marker::Table>>,
    nodes: usize,
    table_entries: usize,
}

impl<'h> ValueVisitor<'h> {
    /// Builds a visitor for `heap`.
    #[must_use]
    pub fn new(
        heap: &'h Heap,
        payloads: &'h crate::host_type::HostPayloadStore,
        limits: ValueMarshalLimits,
    ) -> Self {
        Self {
            heap,
            payloads,
            limits,
            path: Vec::new(),
            active_tables: Vec::new(),
            nodes: 0,
            table_entries: 0,
        }
    }

    /// Copies a sequence of returned values out of the VM.
    ///
    /// # Errors
    /// Returns [`ValueMarshalError`] when a handle no longer resolves, a cycle or
    /// explicit limit is hit, or host-side allocation fails.
    pub fn visit_values(
        &mut self,
        values: &[RawValue],
    ) -> Result<Vec<ValueSnapshot>, ValueMarshalError> {
        let mut out = Vec::new();
        out.try_reserve(values.len())
            .map_err(|_| ValueMarshalError::new("$", "out of memory marshaling result values"))?;
        for (index, &value) in values.iter().enumerate() {
            self.path.push(format!("[{}]", index + 1));
            let value = self.visit_value_at(value, 0);
            self.path.pop();
            out.push(value?);
        }
        Ok(out)
    }

    /// Copies one value out of the VM.
    ///
    /// # Errors
    /// Returns [`ValueMarshalError`] when a handle no longer resolves, a cycle or
    /// explicit limit is hit, or host-side allocation fails.
    pub fn visit_value(&mut self, value: RawValue) -> Result<ValueSnapshot, ValueMarshalError> {
        self.visit_value_at(value, 0)
    }

    fn visit_value_at(
        &mut self,
        value: RawValue,
        depth: usize,
    ) -> Result<ValueSnapshot, ValueMarshalError> {
        self.bump_node()?;
        match value {
            RawValue::Nil => Ok(ValueSnapshot::Nil),
            RawValue::Boolean(value) => Ok(ValueSnapshot::Boolean(value)),
            RawValue::Number(value) => Ok(ValueSnapshot::Number(value)),
            RawValue::Integer(value) => Ok(ValueSnapshot::Integer(value)),
            RawValue::Vector(value) => Ok(ValueSnapshot::Vector(value)),
            RawValue::LightUserdata { handle, tag } => {
                Ok(ValueSnapshot::LightUserdata { handle, tag })
            }
            RawValue::String(handle) => self.visit_string(handle),
            RawValue::Buffer(handle) => self.visit_buffer(handle),
            RawValue::Table(handle) => self.visit_table(handle, depth),
            RawValue::Function(_) => Ok(ValueSnapshot::Opaque("function")),
            RawValue::Userdata(handle) => self.visit_userdata(handle),
            RawValue::Thread(_) => Ok(ValueSnapshot::Opaque("thread")),
        }
    }

    fn visit_userdata(
        &self,
        handle: RawGc<marker::Userdata>,
    ) -> Result<ValueSnapshot, ValueMarshalError> {
        let userdata = self
            .heap
            .userdata(handle)
            .ok_or_else(|| self.error("userdata handle no longer resolves"))?;
        let Some(host_type) = self.heap.host_types().get(userdata.type_index() as usize) else {
            return Err(self.error("userdata host type no longer resolves"));
        };
        let Some(marshal) = host_type.marshal.as_ref() else {
            return Ok(ValueSnapshot::Opaque("userdata"));
        };
        marshal(self.heap, self.payloads, handle).map_err(|message| self.error(message))
    }

    fn visit_string(&self, handle: RawGc<marker::Str>) -> Result<ValueSnapshot, ValueMarshalError> {
        let string = self
            .heap
            .string(handle)
            .ok_or_else(|| self.error("string handle no longer resolves"))?;
        let bytes = string.bytes();
        if bytes.len() > self.limits.max_string_bytes {
            return Err(self.error(format!(
                "string is {} bytes, over the {}-byte marshal cap",
                bytes.len(),
                self.limits.max_string_bytes
            )));
        }
        let mut out = Vec::new();
        out.try_reserve(bytes.len())
            .map_err(|_| self.error("out of memory marshaling string bytes"))?;
        out.extend_from_slice(bytes);
        Ok(ValueSnapshot::String(out))
    }

    fn visit_buffer(
        &self,
        handle: RawGc<marker::Buffer>,
    ) -> Result<ValueSnapshot, ValueMarshalError> {
        let buffer = self
            .heap
            .buffer(handle)
            .ok_or_else(|| self.error("buffer handle no longer resolves"))?;
        let bytes = buffer.bytes();
        if bytes.len() > self.limits.max_buffer_bytes {
            return Err(self.error(format!(
                "buffer is {} bytes, over the {}-byte marshal cap",
                bytes.len(),
                self.limits.max_buffer_bytes
            )));
        }
        let mut out = Vec::new();
        out.try_reserve(bytes.len())
            .map_err(|_| self.error("out of memory marshaling buffer bytes"))?;
        out.extend_from_slice(bytes);
        Ok(ValueSnapshot::Buffer(out))
    }

    fn visit_table(
        &mut self,
        handle: RawGc<marker::Table>,
        depth: usize,
    ) -> Result<ValueSnapshot, ValueMarshalError> {
        if depth >= self.limits.max_depth {
            return Err(self.error(format!(
                "value depth exceeds marshal cap {}",
                self.limits.max_depth
            )));
        }
        if self.active_tables.contains(&handle) {
            return Err(self.error("table cycle cannot be marshaled"));
        }
        let table = self
            .heap
            .table(handle)
            .ok_or_else(|| self.error("table handle no longer resolves"))?;
        let json_array_marker = self.table_has_json_array_marker(table)?;

        let mut raw_pairs = Vec::new();
        let mut pair_count = 0usize;
        table.for_each_entry(|key, value| {
            pair_count += 1;
            raw_pairs.push((key, value));
        });
        let marshaled_pair_count = pair_count + usize::from(json_array_marker);
        self.table_entries = self
            .table_entries
            .checked_add(marshaled_pair_count)
            .ok_or_else(|| self.error("table entry count overflowed while marshaling"))?;
        if self.table_entries > self.limits.max_table_entries {
            return Err(self.error(format!(
                "table entries exceed marshal cap {}",
                self.limits.max_table_entries
            )));
        }

        let mut pairs = Vec::new();
        pairs
            .try_reserve(raw_pairs.len() + usize::from(json_array_marker))
            .map_err(|_| self.error("out of memory marshaling table entries"))?;
        if json_array_marker {
            self.bump_node()?;
            self.bump_node()?;
            pairs.push(json_array_marker_pair());
        }
        self.active_tables.push(handle);
        for (index, (key, value)) in raw_pairs.into_iter().enumerate() {
            self.path.push(format!(".pair{}", index + 1));
            self.path.push(".key".to_owned());
            let key = self.visit_value_at(key, depth + 1);
            self.path.pop();
            self.path.pop();
            let key = key?;
            self.path.push(table_value_path(&key, index));
            let value = self.visit_value_at(value, depth + 1);
            self.path.pop();
            pairs.push(MarshaledPair { key, value: value? });
        }
        self.active_tables.pop();
        Ok(ValueSnapshot::Table(pairs))
    }

    fn table_has_json_array_marker(&self, table: &LuaTable) -> Result<bool, ValueMarshalError> {
        let Some(metatable_handle) = table.metatable() else {
            return Ok(false);
        };
        let metatable = self
            .heap
            .table(metatable_handle)
            .ok_or_else(|| self.error("table metatable handle no longer resolves"))?;
        let mut marker = Ok(false);
        metatable.for_each_entry(|key, value| {
            if marker.is_err() || marker == Ok(true) {
                return;
            }
            let RawValue::String(key) = key else {
                return;
            };
            marker = match self.heap.string(key) {
                Some(key) if key.bytes() == JSON_ARRAY_MARKER_KEY.as_bytes() => {
                    Ok(is_json_array_marker(value))
                }
                Some(_) => Ok(false),
                None => Err(self.error("metatable string key handle no longer resolves")),
            };
        });
        marker
    }

    fn bump_node(&mut self) -> Result<(), ValueMarshalError> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or_else(|| self.error("value count overflowed while marshaling"))?;
        if self.nodes > self.limits.max_nodes {
            return Err(self.error(format!(
                "value count exceeds marshal cap {}",
                self.limits.max_nodes
            )));
        }
        Ok(())
    }

    fn error(&self, message: impl Into<String>) -> ValueMarshalError {
        let mut path = String::from("$");
        for part in &self.path {
            path.push_str(part);
        }
        ValueMarshalError::new(&path, message)
    }
}

/// Render the path segment for one table value using its script-facing key when possible.
fn table_value_path(key: &ValueSnapshot, pair_index: usize) -> String {
    match key {
        ValueSnapshot::String(bytes) => str::from_utf8(bytes).map_or_else(
            |_| format!(".pair{}.value", pair_index + 1),
            |key| {
                if is_identifier(key) {
                    format!(".{key}")
                } else {
                    format!(
                        "[{}]",
                        serde_json::to_string(key).expect("strings serialize to JSON")
                    )
                }
            },
        ),
        ValueSnapshot::Integer(index) => format!("[{index}]"),
        ValueSnapshot::Number(index) if index.is_finite() && index.fract() == 0.0 => {
            format!("[{index:.0}]")
        }
        _ => format!(".pair{}.value", pair_index + 1),
    }
}

/// Return whether a string can use ordinary field syntax in a diagnostic path.
fn is_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn is_json_array_marker(value: RawValue) -> bool {
    matches!(
        value,
        RawValue::LightUserdata {
            handle: JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE,
            tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
        }
    )
}

fn json_array_marker_pair() -> MarshaledPair {
    MarshaledPair {
        key: ValueSnapshot::LightUserdata {
            handle: JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE,
            tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
        },
        value: ValueSnapshot::Boolean(true),
    }
}

#[cfg(any())]
mod tests {
    use super::{MarshaledPair, ValueAccessError, ValueSnapshot, table_value_path};

    #[test]
    fn typed_table_fields_are_ordered_borrowed_and_strict() {
        let value = ValueSnapshot::Table(vec![
            MarshaledPair {
                key: ValueSnapshot::Integer(1),
                value: ValueSnapshot::String(b"ignored".to_vec()),
            },
            MarshaledPair {
                key: ValueSnapshot::String(b"name".to_vec()),
                value: ValueSnapshot::String(b"first".to_vec()),
            },
            MarshaledPair {
                key: ValueSnapshot::String(b"name".to_vec()),
                value: ValueSnapshot::String(b"second".to_vec()),
            },
        ]);
        let borrowed = value.str_field("name").expect("string field").unwrap();
        assert_eq!(borrowed, "first");
        assert_eq!(value.table_field("missing").expect("missing field"), None);

        let wrong = ValueSnapshot::Table(vec![MarshaledPair {
            key: ValueSnapshot::String(b"name".to_vec()),
            value: ValueSnapshot::Integer(i64::MAX),
        }]);
        assert!(matches!(
            wrong.str_field("name"),
            Err(ValueAccessError::ExpectedString {
                actual: "number",
                ..
            })
        ));

        let invalid = ValueSnapshot::Table(vec![MarshaledPair {
            key: ValueSnapshot::String(b"name".to_vec()),
            value: ValueSnapshot::String(vec![0xff]),
        }]);
        assert!(matches!(
            invalid.str_field("name"),
            Err(ValueAccessError::InvalidUtf8 { .. })
        ));
        assert!(matches!(
            ValueSnapshot::Integer(i64::MAX).as_table(),
            Err(ValueAccessError::ExpectedTable { actual: "number" })
        ));
    }

    #[test]
    fn marshaled_values_round_trip_through_serde() {
        let value = ValueSnapshot::Table(vec![
            MarshaledPair {
                key: ValueSnapshot::String(b"k".to_vec()),
                value: ValueSnapshot::Integer(7),
            },
            MarshaledPair {
                key: ValueSnapshot::Number(1.5),
                value: ValueSnapshot::Vector([1.0, 2.0, 3.0]),
            },
            MarshaledPair {
                key: ValueSnapshot::Boolean(true),
                value: ValueSnapshot::Opaque("function"),
            },
        ]);
        let encoded = serde_json::to_string(&value).expect("serialize");
        let decoded: ValueSnapshot = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, value);
    }

    #[test]
    fn an_unknown_opaque_kind_decodes_to_the_generic_kind() {
        let decoded: ValueSnapshot =
            serde_json::from_str(r#"{"Opaque":"mystery"}"#).expect("deserialize");
        assert_eq!(decoded, ValueSnapshot::Opaque("opaque"));
    }

    #[test]
    fn marshaled_value_display_lua_covers_owned_kinds() {
        let values = [
            (ValueSnapshot::Nil, "nil"),
            (ValueSnapshot::Boolean(false), "false"),
            (ValueSnapshot::Number(2.0), "2"),
            (ValueSnapshot::Number(-0.0), "-0"),
            (ValueSnapshot::Integer(4), "4"),
            (ValueSnapshot::Vector([1.0, 2.5, -0.0]), "1, 2.5, -0"),
            (
                ValueSnapshot::LightUserdata { handle: 1, tag: 2 },
                "userdata",
            ),
            (ValueSnapshot::String(b"hello".to_vec()), "hello"),
            (ValueSnapshot::Buffer(b"bytes".to_vec()), "buffer"),
            (ValueSnapshot::Table(Vec::new()), "table"),
            (ValueSnapshot::Opaque("function"), "function"),
        ];

        for (value, display) in values {
            assert_eq!(value.display_lua(), display, "{value:?}");
        }
    }

    #[test]
    fn table_value_paths_prefer_script_facing_keys() {
        assert_eq!(
            table_value_path(&ValueSnapshot::String(b"field".to_vec()), 0),
            ".field"
        );
        assert_eq!(
            table_value_path(&ValueSnapshot::String(b"not a field".to_vec()), 1),
            "[\"not a field\"]"
        );
        assert_eq!(table_value_path(&ValueSnapshot::Integer(3), 2), "[3]");
        assert_eq!(table_value_path(&ValueSnapshot::Number(4.0), 3), "[4]");
        assert_eq!(
            table_value_path(&ValueSnapshot::Boolean(true), 4),
            ".pair5.value"
        );
    }
}
