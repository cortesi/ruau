//! Owned value marshaling for values leaving a VM entry point.
//!
//! Host returns use `OwnedValue` because they may need to materialize registry
//! pins back into the VM. Entry results flow the other way: they must be plain
//! owned data, with no raw or registry handles escaping after the VM borrow ends.

use ruau_vm_api::{RawGc, RawValue, marker};

use crate::{
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
pub enum MarshaledValue {
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

impl MarshaledValue {
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
}

/// One owned table key/value pair.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct MarshaledPair {
    /// Copied table key.
    pub key: MarshaledValue,
    /// Copied table value.
    pub value: MarshaledValue,
}

impl<'de> serde::Deserialize<'de> for MarshaledValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Owned mirror of [`MarshaledValue`] for deserialization; `Opaque`
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
    limits: ValueMarshalLimits,
    path: Vec<String>,
    active_tables: Vec<RawGc<marker::Table>>,
    nodes: usize,
    table_entries: usize,
}

impl<'h> ValueVisitor<'h> {
    /// Builds a visitor for `heap`.
    #[must_use]
    pub fn new(heap: &'h Heap, limits: ValueMarshalLimits) -> Self {
        Self {
            heap,
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
    ) -> Result<Vec<MarshaledValue>, ValueMarshalError> {
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
    pub fn visit_value(&mut self, value: RawValue) -> Result<MarshaledValue, ValueMarshalError> {
        self.visit_value_at(value, 0)
    }

    fn visit_value_at(
        &mut self,
        value: RawValue,
        depth: usize,
    ) -> Result<MarshaledValue, ValueMarshalError> {
        self.bump_node()?;
        match value {
            RawValue::Nil => Ok(MarshaledValue::Nil),
            RawValue::Boolean(value) => Ok(MarshaledValue::Boolean(value)),
            RawValue::Number(value) => Ok(MarshaledValue::Number(value)),
            RawValue::Integer(value) => Ok(MarshaledValue::Integer(value)),
            RawValue::Vector(value) => Ok(MarshaledValue::Vector(value)),
            RawValue::LightUserdata { handle, tag } => {
                Ok(MarshaledValue::LightUserdata { handle, tag })
            }
            RawValue::String(handle) => self.visit_string(handle),
            RawValue::Buffer(handle) => self.visit_buffer(handle),
            RawValue::Table(handle) => self.visit_table(handle, depth),
            RawValue::Function(_) => Ok(MarshaledValue::Opaque("function")),
            RawValue::Userdata(handle) => self.visit_userdata(handle),
            RawValue::Thread(_) => Ok(MarshaledValue::Opaque("thread")),
        }
    }

    fn visit_userdata(
        &self,
        handle: RawGc<marker::Userdata>,
    ) -> Result<MarshaledValue, ValueMarshalError> {
        let userdata = self
            .heap
            .userdata(handle)
            .ok_or_else(|| self.error("userdata handle no longer resolves"))?;
        let Some(host_type) = self.heap.host_types().get(userdata.type_index() as usize) else {
            return Err(self.error("userdata host type no longer resolves"));
        };
        let Some(marshal) = host_type.marshal.as_ref() else {
            return Ok(MarshaledValue::Opaque("userdata"));
        };
        marshal(self.heap, handle).map_err(|message| self.error(message))
    }

    fn visit_string(
        &self,
        handle: RawGc<marker::Str>,
    ) -> Result<MarshaledValue, ValueMarshalError> {
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
        Ok(MarshaledValue::String(out))
    }

    fn visit_buffer(
        &self,
        handle: RawGc<marker::Buffer>,
    ) -> Result<MarshaledValue, ValueMarshalError> {
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
        Ok(MarshaledValue::Buffer(out))
    }

    fn visit_table(
        &mut self,
        handle: RawGc<marker::Table>,
        depth: usize,
    ) -> Result<MarshaledValue, ValueMarshalError> {
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
            self.path.push(".value".to_owned());
            let value = self.visit_value_at(value, depth + 1);
            self.path.pop();
            self.path.pop();
            pairs.push(MarshaledPair {
                key: key?,
                value: value?,
            });
        }
        self.active_tables.pop();
        Ok(MarshaledValue::Table(pairs))
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
        key: MarshaledValue::LightUserdata {
            handle: JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE,
            tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
        },
        value: MarshaledValue::Boolean(true),
    }
}

#[cfg(any())]
mod tests {
    use super::{MarshaledPair, MarshaledValue};

    #[test]
    fn marshaled_values_round_trip_through_serde() {
        let value = MarshaledValue::Table(vec![
            MarshaledPair {
                key: MarshaledValue::String(b"k".to_vec()),
                value: MarshaledValue::Integer(7),
            },
            MarshaledPair {
                key: MarshaledValue::Number(1.5),
                value: MarshaledValue::Vector([1.0, 2.0, 3.0]),
            },
            MarshaledPair {
                key: MarshaledValue::Boolean(true),
                value: MarshaledValue::Opaque("function"),
            },
        ]);
        let encoded = serde_json::to_string(&value).expect("serialize");
        let decoded: MarshaledValue = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, value);
    }

    #[test]
    fn an_unknown_opaque_kind_decodes_to_the_generic_kind() {
        let decoded: MarshaledValue =
            serde_json::from_str(r#"{"Opaque":"mystery"}"#).expect("deserialize");
        assert_eq!(decoded, MarshaledValue::Opaque("opaque"));
    }

    #[test]
    fn marshaled_value_display_lua_covers_owned_kinds() {
        let values = [
            (MarshaledValue::Nil, "nil"),
            (MarshaledValue::Boolean(false), "false"),
            (MarshaledValue::Number(2.0), "2"),
            (MarshaledValue::Number(-0.0), "-0"),
            (MarshaledValue::Integer(4), "4"),
            (MarshaledValue::Vector([1.0, 2.5, -0.0]), "1, 2.5, -0"),
            (
                MarshaledValue::LightUserdata { handle: 1, tag: 2 },
                "userdata",
            ),
            (MarshaledValue::String(b"hello".to_vec()), "hello"),
            (MarshaledValue::Buffer(b"bytes".to_vec()), "buffer"),
            (MarshaledValue::Table(Vec::new()), "table"),
            (MarshaledValue::Opaque("function"), "function"),
        ];

        for (value, display) in values {
            assert_eq!(value.display_lua(), display, "{value:?}");
        }
    }
}
