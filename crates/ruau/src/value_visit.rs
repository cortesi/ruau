//! Generic traversal helpers for values crossing a Luau host boundary.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    os::raw::c_void,
};

use crate::{
    AnyUserData, Buffer, Error, Function, Integer, LightUserData, Luau, LuauString, Number, Table, Thread,
    Value, Vector,
};

/// Result type returned by value-boundary visitors.
pub type ValueVisitResult<T> = std::result::Result<T, ValueVisitError>;

/// A path to a value while traversing a host boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValuePath {
    /// Root label, such as `value` or `argument 1`.
    root: String,
    /// Child path segments from root to current value.
    segments: Vec<ValuePathSegment>,
}

impl ValuePath {
    /// Creates a path with a caller-defined root label.
    #[must_use]
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            segments: Vec::new(),
        }
    }

    /// Creates a path rooted at `value`.
    #[must_use]
    pub fn value() -> Self {
        Self::new("value")
    }

    /// Creates a path rooted at a 1-based argument position.
    #[must_use]
    pub fn argument(position: usize) -> Self {
        Self::new(format!("argument {position}"))
    }

    /// Returns a copy of this path with an array index appended.
    #[must_use]
    pub fn indexed(&self, index: usize) -> Self {
        let mut path = self.clone();
        path.push_index(index);
        path
    }

    /// Returns a copy of this path with a map field appended.
    #[must_use]
    pub fn field(&self, field: impl Into<String>) -> Self {
        let mut path = self.clone();
        path.push_field(field);
        path
    }

    /// Appends an array index in place.
    fn push_index(&mut self, index: usize) {
        self.segments.push(ValuePathSegment::Index(index));
    }

    /// Appends a map field in place.
    fn push_field(&mut self, field: impl Into<String>) {
        self.segments.push(ValuePathSegment::Field(field.into()));
    }
}

impl Default for ValuePath {
    fn default() -> Self {
        Self::value()
    }
}

impl fmt::Display for ValuePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.root)?;
        for segment in &self.segments {
            match segment {
                ValuePathSegment::Index(index) => write!(formatter, "[{index}]")?,
                ValuePathSegment::Field(field) if is_luau_identifier(field) => {
                    write!(formatter, ".{field}")?;
                }
                ValuePathSegment::Field(field) => write!(formatter, "[{field:?}]")?,
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Segment in a [`ValuePath`].
enum ValuePathSegment {
    /// One-based array index.
    Index(usize),
    /// String map key.
    Field(String),
}

/// A typed failure encountered while traversing a value boundary.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ValueVisitError {
    /// A table cycle was found after host hooks declined to handle the table.
    #[error("cycle detected at {path}")]
    Cycle {
        /// Path to the repeated table.
        path: ValuePath,
    },
    /// An array table has a missing positive integer index.
    #[error("sparse array at {path}: missing index {index}")]
    SparseArray {
        /// Path to the array table.
        path: ValuePath,
        /// The missing 1-based array index.
        index: usize,
    },
    /// A table contains both positive integer keys and string keys.
    #[error("mixed array/map table at {path}: found {found_key_type} key after {existing_key_type} keys")]
    MixedTableKeys {
        /// Path to the table.
        path: ValuePath,
        /// The key family already seen in the table.
        existing_key_type: &'static str,
        /// The key family that conflicts with the existing table shape.
        found_key_type: &'static str,
    },
    /// A table key is not supported by the boundary rules.
    #[error("unsupported table key at {path}: {key_type}")]
    UnsupportedTableKey {
        /// Path to the table containing the key.
        path: ValuePath,
        /// Type name of the unsupported key.
        key_type: &'static str,
    },
    /// A string table key is not valid UTF-8.
    #[error("non-UTF-8 table key at {path}")]
    NonUtf8TableKey {
        /// Path to the table containing the key.
        path: ValuePath,
    },
    /// A value shape is not supported by the boundary rules.
    #[error("unsupported {type_name} at {path}")]
    UnsupportedValue {
        /// Path to the unsupported value.
        path: ValuePath,
        /// Type name of the unsupported value.
        type_name: &'static str,
    },
    /// Luau failed while constructing or reading a value.
    #[error("Luau error at {path}: {source}")]
    Luau {
        /// Path to the value being processed.
        path: ValuePath,
        /// Underlying Luau error.
        source: Error,
    },
    /// A host visitor rejected a value.
    #[error("{message} at {path}")]
    Custom {
        /// Path to the rejected value.
        path: ValuePath,
        /// Human-readable error message.
        message: String,
    },
}

impl ValueVisitError {
    /// Returns the path associated with this error.
    #[must_use]
    pub const fn path(&self) -> &ValuePath {
        match self {
            Self::Cycle { path }
            | Self::SparseArray { path, .. }
            | Self::MixedTableKeys { path, .. }
            | Self::UnsupportedTableKey { path, .. }
            | Self::NonUtf8TableKey { path }
            | Self::UnsupportedValue { path, .. }
            | Self::Luau { path, .. }
            | Self::Custom { path, .. } => path,
        }
    }

    /// Creates a path-attributed host error.
    #[must_use]
    pub fn custom(path: &ValuePath, message: impl Into<String>) -> Self {
        Self::Custom {
            path: path.clone(),
            message: message.into(),
        }
    }

    /// Wraps a Luau error at a traversal path.
    fn luau(path: &ValuePath, source: Error) -> Self {
        Self::Luau {
            path: path.clone(),
            source,
        }
    }
}

/// Result of a host boundary hook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoundaryAction<T> {
    /// The host handled this value and supplied the visitor output.
    Replace(T),
    /// The generic visitor should descend into the value normally.
    Descend,
}

/// A Luau host handle seen while visiting an outbound value.
#[non_exhaustive]
#[derive(Clone, Copy)]
pub enum HostValue<'a> {
    /// Light userdata.
    LightUserData(&'a LightUserData),
    /// Function handle.
    Function(&'a Function),
    /// Thread handle.
    Thread(&'a Thread),
    /// Userdata handle.
    UserData(&'a AnyUserData),
}

impl HostValue<'_> {
    /// Returns the Luau type name for this handle.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::LightUserData(_) => "lightuserdata",
            Self::Function(_) => "function",
            Self::Thread(_) => "thread",
            Self::UserData(_) => "userdata",
        }
    }
}

/// A value shape that the generic outbound visitor does not encode by default.
#[non_exhaustive]
pub enum UnsupportedOutboundValue<'a> {
    /// A non-finite floating-point number.
    NonFiniteNumber(Number),
    /// A vector value.
    Vector(&'a Vector),
    /// A host handle that was not replaced by [`OutboundVisitor::host_value`].
    Host(HostValue<'a>),
    /// A Luau error value.
    Error(&'a Error),
    /// An opaque Luau value.
    Other(&'a Value),
}

impl UnsupportedOutboundValue<'_> {
    /// Returns the Luau type name for this value.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::NonFiniteNumber(_) => "non-finite number",
            Self::Vector(_) => "vector",
            Self::Host(value) => value.type_name(),
            Self::Error(_) => "error",
            Self::Other(_) => "other",
        }
    }
}

/// Host policy for walking a Luau value into an arbitrary output representation.
pub trait OutboundVisitor {
    /// Output value produced by the visitor.
    type Output;

    /// Visits `nil`.
    fn nil(&mut self, path: &ValuePath) -> ValueVisitResult<Self::Output>;

    /// Visits a boolean.
    fn boolean(&mut self, value: bool, path: &ValuePath) -> ValueVisitResult<Self::Output>;

    /// Visits an integer.
    fn integer(&mut self, value: Integer, path: &ValuePath) -> ValueVisitResult<Self::Output>;

    /// Visits a finite floating-point number.
    fn number(&mut self, value: Number, path: &ValuePath) -> ValueVisitResult<Self::Output>;

    /// Visits a Luau string.
    fn string(&mut self, value: &LuauString, path: &ValuePath) -> ValueVisitResult<Self::Output>;

    /// Visits a Luau buffer.
    fn buffer(&mut self, value: &Buffer, path: &ValuePath) -> ValueVisitResult<Self::Output>;

    /// Gives the host a chance to encode a table before generic traversal.
    fn table(&mut self, _table: &Table, _path: &ValuePath) -> ValueVisitResult<BoundaryAction<Self::Output>> {
        Ok(BoundaryAction::Descend)
    }

    /// Gives the host a chance to encode a handle before the unsupported policy runs.
    fn host_value(
        &mut self,
        _value: HostValue<'_>,
        _path: &ValuePath,
    ) -> ValueVisitResult<BoundaryAction<Self::Output>> {
        Ok(BoundaryAction::Descend)
    }

    /// Visits an array table after all elements have been visited.
    fn array(&mut self, values: Vec<Self::Output>, path: &ValuePath) -> ValueVisitResult<Self::Output>;

    /// Visits a map table after all entries have been visited.
    fn map(
        &mut self,
        entries: Vec<(String, Self::Output)>,
        path: &ValuePath,
    ) -> ValueVisitResult<Self::Output>;

    /// Handles a value shape that has no generic outbound encoding.
    fn unsupported(
        &mut self,
        value: UnsupportedOutboundValue<'_>,
        path: &ValuePath,
    ) -> ValueVisitResult<Self::Output> {
        Err(ValueVisitError::UnsupportedValue {
            path: path.clone(),
            type_name: value.type_name(),
        })
    }
}

/// Visits a Luau value with a host-defined outbound policy.
pub fn visit_luau_value<V: OutboundVisitor>(value: &Value, visitor: &mut V) -> ValueVisitResult<V::Output> {
    visit_luau_value_at_path(value, ValuePath::value(), visitor)
}

/// Visits a Luau value with a caller-supplied root path.
pub fn visit_luau_value_at_path<V: OutboundVisitor>(
    value: &Value,
    path: impl Into<ValuePath>,
    visitor: &mut V,
) -> ValueVisitResult<V::Output> {
    let path = path.into();
    let mut active_tables = HashSet::new();
    visit_luau_value_inner(value, visitor, &path, &mut active_tables)
}

/// A generic inbound value source.
pub trait InboundSource: Sized {
    /// Returns the source shape to convert at the current path.
    fn inbound_kind(&self, path: &ValuePath) -> ValueVisitResult<InboundKind<'_, Self>>;
}

/// A generic inbound value shape.
#[non_exhaustive]
pub enum InboundKind<'a, S: InboundSource> {
    /// Nil.
    Nil,
    /// Boolean.
    Boolean(bool),
    /// Integer.
    Integer(Integer),
    /// Floating-point number.
    Number(Number),
    /// UTF-8 text.
    String(&'a str),
    /// Binary payload.
    Binary(&'a [u8]),
    /// Array children.
    Array(Vec<&'a S>),
    /// Map children.
    Map(Vec<(InboundMapKey<'a>, &'a S)>),
    /// Unsupported source shape.
    Unsupported(&'static str),
}

/// A generic inbound map key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InboundMapKey<'a> {
    /// UTF-8 string key.
    String(&'a str),
    /// Unsupported key shape.
    Unsupported(&'static str),
}

/// Host policy for converting a generic inbound value into Luau.
pub trait InboundVisitor<S: InboundSource> {
    /// Gives the host a chance to replace a map before generic conversion.
    fn map(
        &mut self,
        _entries: &[(InboundMapKey<'_>, &S)],
        _lua: &Luau,
        _path: &ValuePath,
    ) -> ValueVisitResult<BoundaryAction<Value>> {
        Ok(BoundaryAction::Descend)
    }
}

/// Inbound visitor that applies only the generic conversion rules.
#[derive(Default)]
pub struct DefaultInboundVisitor;

impl<S: InboundSource> InboundVisitor<S> for DefaultInboundVisitor {}

/// Converts a generic inbound value into a Luau value.
pub fn inbound_to_luau<S: InboundSource, V: InboundVisitor<S>>(
    lua: &Luau,
    source: &S,
    visitor: &mut V,
) -> ValueVisitResult<Value> {
    inbound_to_luau_at_path(lua, source, ValuePath::value(), visitor)
}

/// Converts a generic inbound value into a Luau value with a caller-supplied root path.
pub fn inbound_to_luau_at_path<S: InboundSource, V: InboundVisitor<S>>(
    lua: &Luau,
    source: &S,
    path: impl Into<ValuePath>,
    visitor: &mut V,
) -> ValueVisitResult<Value> {
    let path = path.into();
    match source.inbound_kind(&path)? {
        InboundKind::Nil => Ok(Value::Nil),
        InboundKind::Boolean(value) => Ok(Value::Boolean(value)),
        InboundKind::Integer(value) => Ok(Value::Integer(value)),
        InboundKind::Number(value) if value.is_finite() => Ok(Value::Number(value)),
        InboundKind::Number(_) => Err(ValueVisitError::UnsupportedValue {
            path,
            type_name: "non-finite number",
        }),
        InboundKind::String(value) => lua
            .create_string(value)
            .map(Value::String)
            .map_err(|error| ValueVisitError::luau(&path, error)),
        InboundKind::Binary(value) => lua
            .create_buffer(value)
            .map(Value::Buffer)
            .map_err(|error| ValueVisitError::luau(&path, error)),
        InboundKind::Array(values) => inbound_array_to_luau(lua, values, &path, visitor),
        InboundKind::Map(entries) => inbound_map_to_luau(lua, entries, &path, visitor),
        InboundKind::Unsupported(type_name) => Err(ValueVisitError::UnsupportedValue { path, type_name }),
    }
}

/// Recursive outbound traversal entrypoint.
fn visit_luau_value_inner<V: OutboundVisitor>(
    value: &Value,
    visitor: &mut V,
    path: &ValuePath,
    active_tables: &mut HashSet<*const c_void>,
) -> ValueVisitResult<V::Output> {
    match value {
        Value::Nil => visitor.nil(path),
        Value::Boolean(value) => visitor.boolean(*value, path),
        Value::Integer(value) => visitor.integer(*value, path),
        Value::Number(value) if value.is_finite() => visitor.number(*value, path),
        Value::Number(value) => visitor.unsupported(UnsupportedOutboundValue::NonFiniteNumber(*value), path),
        Value::Vector(value) => visitor.unsupported(UnsupportedOutboundValue::Vector(value), path),
        Value::String(value) => visitor.string(value, path),
        Value::Table(table) => visit_table(table, visitor, path, active_tables),
        Value::Function(value) => visit_host_value(HostValue::Function(value), visitor, path),
        Value::Thread(value) => visit_host_value(HostValue::Thread(value), visitor, path),
        Value::UserData(value) => visit_host_value(HostValue::UserData(value), visitor, path),
        Value::LightUserData(value) => visit_host_value(HostValue::LightUserData(value), visitor, path),
        Value::Buffer(value) => visitor.buffer(value, path),
        Value::Error(value) => visitor.unsupported(UnsupportedOutboundValue::Error(value), path),
        Value::Other(_) => visitor.unsupported(UnsupportedOutboundValue::Other(value), path),
    }
}

/// Visits a host-owned Luau handle or routes it to the unsupported policy.
fn visit_host_value<V: OutboundVisitor>(
    value: HostValue<'_>,
    visitor: &mut V,
    path: &ValuePath,
) -> ValueVisitResult<V::Output> {
    match visitor.host_value(value, path)? {
        BoundaryAction::Replace(value) => Ok(value),
        BoundaryAction::Descend => visitor.unsupported(UnsupportedOutboundValue::Host(value), path),
    }
}

/// Visits a Luau table with cycle detection and table hook support.
fn visit_table<V: OutboundVisitor>(
    table: &Table,
    visitor: &mut V,
    path: &ValuePath,
    active_tables: &mut HashSet<*const c_void>,
) -> ValueVisitResult<V::Output> {
    match visitor.table(table, path)? {
        BoundaryAction::Replace(value) => return Ok(value),
        BoundaryAction::Descend => {}
    }

    let pointer = table.to_pointer();
    if !active_tables.insert(pointer) {
        return Err(ValueVisitError::Cycle { path: path.clone() });
    }

    let result = visit_table_shape(table, visitor, path, active_tables);
    active_tables.remove(&pointer);
    result
}

/// Visits a Luau table after determining whether it is an array or map.
fn visit_table_shape<V: OutboundVisitor>(
    table: &Table,
    visitor: &mut V,
    path: &ValuePath,
    active_tables: &mut HashSet<*const c_void>,
) -> ValueVisitResult<V::Output> {
    match table_shape(table, path)? {
        TableShape::Array(values) => {
            let mut visited = Vec::with_capacity(values.len());
            for (index, value) in values {
                let child_path = path.indexed(index);
                visited.push(visit_luau_value_inner(
                    &value,
                    visitor,
                    &child_path,
                    active_tables,
                )?);
            }
            visitor.array(visited, path)
        }
        TableShape::Map(entries) => {
            let mut visited = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                let child_path = path.field(&key);
                visited.push((
                    key,
                    visit_luau_value_inner(&value, visitor, &child_path, active_tables)?,
                ));
            }
            visitor.map(visited, path)
        }
    }
}

/// Determines a Luau table's generic boundary shape.
fn table_shape(table: &Table, path: &ValuePath) -> ValueVisitResult<TableShape> {
    let mut array = BTreeMap::new();
    let mut map = BTreeMap::new();

    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|error| ValueVisitError::luau(path, error))?;
        match key {
            Value::Integer(index) if index > 0 => {
                if !map.is_empty() {
                    return Err(ValueVisitError::MixedTableKeys {
                        path: path.clone(),
                        existing_key_type: "string",
                        found_key_type: "integer",
                    });
                }
                array.insert(index as usize, value);
            }
            Value::String(key) => {
                if !array.is_empty() {
                    return Err(ValueVisitError::MixedTableKeys {
                        path: path.clone(),
                        existing_key_type: "integer",
                        found_key_type: "string",
                    });
                }
                let key = key
                    .to_str()
                    .map_err(|_| ValueVisitError::NonUtf8TableKey { path: path.clone() })?
                    .to_owned();
                map.insert(key, value);
            }
            key => {
                return Err(ValueVisitError::UnsupportedTableKey {
                    path: path.clone(),
                    key_type: key.type_name(),
                });
            }
        }
    }

    if map.is_empty() {
        let expected_len = array.len();
        for expected in 1..=expected_len {
            if !array.contains_key(&expected) {
                return Err(ValueVisitError::SparseArray {
                    path: path.clone(),
                    index: expected,
                });
            }
        }
        Ok(TableShape::Array(array.into_iter().collect()))
    } else {
        Ok(TableShape::Map(map.into_iter().collect()))
    }
}

/// Converts an inbound array shape to a Luau table.
fn inbound_array_to_luau<S: InboundSource, V: InboundVisitor<S>>(
    lua: &Luau,
    values: Vec<&S>,
    path: &ValuePath,
    visitor: &mut V,
) -> ValueVisitResult<Value> {
    let table = lua
        .create_table_with_capacity(values.len(), 0)
        .map_err(|error| ValueVisitError::luau(path, error))?;
    for (offset, value) in values.into_iter().enumerate() {
        let index = offset + 1;
        let child_path = path.indexed(index);
        let value = inbound_to_luau_at_path(lua, value, child_path.clone(), visitor)?;
        table
            .raw_set(index, value)
            .map_err(|error| ValueVisitError::luau(&child_path, error))?;
    }
    Ok(Value::Table(table))
}

/// Converts an inbound map shape to a Luau table.
fn inbound_map_to_luau<S: InboundSource, V: InboundVisitor<S>>(
    lua: &Luau,
    entries: Vec<(InboundMapKey<'_>, &S)>,
    path: &ValuePath,
    visitor: &mut V,
) -> ValueVisitResult<Value> {
    match visitor.map(&entries, lua, path)? {
        BoundaryAction::Replace(value) => return Ok(value),
        BoundaryAction::Descend => {}
    }

    let table = lua
        .create_table_with_capacity(0, entries.len())
        .map_err(|error| ValueVisitError::luau(path, error))?;
    for (key, value) in entries {
        let InboundMapKey::String(key) = key else {
            let type_name = match key {
                InboundMapKey::Unsupported(type_name) => type_name,
                InboundMapKey::String(_) => unreachable!("string key handled by let-else"),
            };
            return Err(ValueVisitError::UnsupportedTableKey {
                path: path.clone(),
                key_type: type_name,
            });
        };
        let child_path = path.field(key);
        let value = inbound_to_luau_at_path(lua, value, child_path.clone(), visitor)?;
        table
            .raw_set(key, value)
            .map_err(|error| ValueVisitError::luau(&child_path, error))?;
    }
    Ok(Value::Table(table))
}

/// Generic Luau table shape after boundary classification.
enum TableShape {
    /// Dense one-based array values.
    Array(Vec<(usize, Value)>),
    /// String-keyed map values.
    Map(Vec<(String, Value)>),
}

/// Returns whether `value` can be displayed as a Luau dotted field.
fn is_luau_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|char| char == '_' || char.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;

    #[derive(Debug, PartialEq)]
    enum Seen {
        Nil,
        Bool(bool),
        Int(Integer),
        Number(Number),
        String(Vec<u8>),
        Buffer(Vec<u8>),
        Array(Vec<Self>),
        Map(Vec<(String, Self)>),
        Host(String),
    }

    struct RecordingVisitor {
        paths: Vec<String>,
        handled_tables: HashSet<*const c_void>,
    }

    impl RecordingVisitor {
        fn new() -> Self {
            Self {
                paths: Vec::new(),
                handled_tables: HashSet::new(),
            }
        }

        fn with_handled_table(table: &Table) -> Self {
            let mut visitor = Self::new();
            visitor.handled_tables.insert(table.to_pointer());
            visitor
        }

        fn record(&mut self, path: &ValuePath) {
            self.paths.push(path.to_string());
        }
    }

    impl OutboundVisitor for RecordingVisitor {
        type Output = Seen;

        fn nil(&mut self, path: &ValuePath) -> ValueVisitResult<Self::Output> {
            self.record(path);
            Ok(Seen::Nil)
        }

        fn boolean(&mut self, value: bool, path: &ValuePath) -> ValueVisitResult<Self::Output> {
            self.record(path);
            Ok(Seen::Bool(value))
        }

        fn integer(&mut self, value: Integer, path: &ValuePath) -> ValueVisitResult<Self::Output> {
            self.record(path);
            Ok(Seen::Int(value))
        }

        fn number(&mut self, value: Number, path: &ValuePath) -> ValueVisitResult<Self::Output> {
            self.record(path);
            Ok(Seen::Number(value))
        }

        fn string(&mut self, value: &LuauString, path: &ValuePath) -> ValueVisitResult<Self::Output> {
            self.record(path);
            Ok(Seen::String(value.as_bytes().to_vec()))
        }

        fn buffer(&mut self, value: &Buffer, path: &ValuePath) -> ValueVisitResult<Self::Output> {
            self.record(path);
            Ok(Seen::Buffer(value.to_vec()))
        }

        fn table(
            &mut self,
            table: &Table,
            path: &ValuePath,
        ) -> ValueVisitResult<BoundaryAction<Self::Output>> {
            if self.handled_tables.contains(&table.to_pointer()) {
                self.record(path);
                Ok(BoundaryAction::Replace(Seen::Host(path.to_string())))
            } else {
                Ok(BoundaryAction::Descend)
            }
        }

        fn array(&mut self, values: Vec<Self::Output>, path: &ValuePath) -> ValueVisitResult<Self::Output> {
            self.record(path);
            Ok(Seen::Array(values))
        }

        fn map(
            &mut self,
            entries: Vec<(String, Self::Output)>,
            path: &ValuePath,
        ) -> ValueVisitResult<Self::Output> {
            self.record(path);
            Ok(Seen::Map(entries))
        }
    }

    #[test]
    fn outbound_tracks_paths_through_arrays_and_maps() -> Result<()> {
        let lua = Luau::new();
        let root = lua.create_table()?;
        let foo = lua.create_table()?;
        foo.raw_set(1, true)?;
        foo.raw_set(2, 7)?;
        root.raw_set("foo", foo)?;

        let mut visitor = RecordingVisitor::new();
        let output = visit_luau_value_at_path(&Value::Table(root), ValuePath::argument(1), &mut visitor)
            .expect("visit should succeed");

        assert_eq!(
            output,
            Seen::Map(vec![(
                "foo".to_string(),
                Seen::Array(vec![Seen::Bool(true), Seen::Int(7)])
            )])
        );
        assert_eq!(
            visitor.paths,
            [
                "argument 1.foo[1]",
                "argument 1.foo[2]",
                "argument 1.foo",
                "argument 1"
            ]
        );
        Ok(())
    }

    #[test]
    fn outbound_reports_sparse_array_path() -> Result<()> {
        let lua = Luau::new();
        let table = lua.create_table()?;
        table.raw_set(1, "first")?;
        table.raw_set(3, "third")?;

        let mut visitor = RecordingVisitor::new();
        let error =
            visit_luau_value(&Value::Table(table), &mut visitor).expect_err("sparse array should fail");

        assert!(matches!(error, ValueVisitError::SparseArray { index: 2, .. }));
        assert_eq!(error.path().to_string(), "value");
        Ok(())
    }

    #[test]
    fn outbound_reports_mixed_table_path() -> Result<()> {
        let lua = Luau::new();
        let table = lua.create_table()?;
        table.raw_set(1, "first")?;
        table.raw_set("name", "second")?;

        let mut visitor = RecordingVisitor::new();
        let error =
            visit_luau_value(&Value::Table(table), &mut visitor).expect_err("mixed table should fail");

        assert!(matches!(error, ValueVisitError::MixedTableKeys { .. }));
        assert_eq!(error.path().to_string(), "value");
        Ok(())
    }

    #[test]
    fn outbound_detects_table_cycles_after_table_hook() -> Result<()> {
        let lua = Luau::new();
        let table = lua.create_table()?;
        table.raw_set("self", table.clone())?;

        let mut visitor = RecordingVisitor::new();
        let error =
            visit_luau_value(&Value::Table(table.clone()), &mut visitor).expect_err("cycle should fail");

        assert!(matches!(error, ValueVisitError::Cycle { .. }));
        assert_eq!(error.path().to_string(), "value.self");

        let mut visitor = RecordingVisitor::with_handled_table(&table);
        let output = visit_luau_value(&Value::Table(table), &mut visitor)
            .expect("table hook should short-circuit cycle detection");
        assert_eq!(output, Seen::Host("value".to_string()));
        Ok(())
    }

    #[test]
    fn outbound_host_value_hook_can_replace_userdata() -> Result<()> {
        #[derive(Clone)]
        struct Handle;

        impl crate::UserData for Handle {}

        struct HandleVisitor;

        impl OutboundVisitor for HandleVisitor {
            type Output = String;

            fn nil(&mut self, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
                Ok("nil".to_string())
            }

            fn boolean(&mut self, _value: bool, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
                Ok("boolean".to_string())
            }

            fn integer(&mut self, _value: Integer, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
                Ok("integer".to_string())
            }

            fn number(&mut self, _value: Number, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
                Ok("number".to_string())
            }

            fn string(&mut self, _value: &LuauString, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
                Ok("string".to_string())
            }

            fn buffer(&mut self, _value: &Buffer, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
                Ok("buffer".to_string())
            }

            fn host_value(
                &mut self,
                value: HostValue<'_>,
                path: &ValuePath,
            ) -> ValueVisitResult<BoundaryAction<Self::Output>> {
                assert!(matches!(value, HostValue::UserData(_)));
                Ok(BoundaryAction::Replace(format!("handle:{path}")))
            }

            fn array(
                &mut self,
                _values: Vec<Self::Output>,
                _path: &ValuePath,
            ) -> ValueVisitResult<Self::Output> {
                Ok("array".to_string())
            }

            fn map(
                &mut self,
                _entries: Vec<(String, Self::Output)>,
                _path: &ValuePath,
            ) -> ValueVisitResult<Self::Output> {
                Ok("map".to_string())
            }
        }

        let lua = Luau::new();
        let userdata = lua.create_userdata(Handle)?;
        let output = visit_luau_value(&Value::UserData(userdata), &mut HandleVisitor)
            .expect("host hook should replace userdata");

        assert_eq!(output, "handle:value");
        Ok(())
    }

    enum Source {
        Nil,
        Int(Integer),
        Text(String),
        Binary(Vec<u8>),
        Array(Vec<Self>),
        Map(Vec<(InboundKey, Self)>),
    }

    enum InboundKey {
        String(String),
        Unsupported(&'static str),
    }

    impl InboundSource for Source {
        fn inbound_kind(&self, _path: &ValuePath) -> ValueVisitResult<InboundKind<'_, Self>> {
            Ok(match self {
                Self::Nil => InboundKind::Nil,
                Self::Int(value) => InboundKind::Integer(*value),
                Self::Text(value) => InboundKind::String(value),
                Self::Binary(value) => InboundKind::Binary(value),
                Self::Array(values) => InboundKind::Array(values.iter().collect()),
                Self::Map(entries) => InboundKind::Map(
                    entries
                        .iter()
                        .map(|(key, value)| {
                            let key = match key {
                                InboundKey::String(key) => InboundMapKey::String(key),
                                InboundKey::Unsupported(type_name) => InboundMapKey::Unsupported(type_name),
                            };
                            (key, value)
                        })
                        .collect(),
                ),
            })
        }
    }

    #[test]
    fn inbound_converts_arrays_maps_and_binary() -> Result<()> {
        let lua = Luau::new();
        let source = Source::Map(vec![
            (
                InboundKey::String("items".to_string()),
                Source::Array(vec![Source::Int(1), Source::Text("two".to_string())]),
            ),
            (
                InboundKey::String("payload".to_string()),
                Source::Binary(vec![1, 2, 3]),
            ),
        ]);

        let value = inbound_to_luau(&lua, &source, &mut DefaultInboundVisitor)
            .expect("inbound conversion should succeed");

        let Value::Table(table) = value else {
            panic!("expected table");
        };
        let items: Table = table.raw_get("items")?;
        assert_eq!(items.raw_get::<Integer>(1)?, 1);
        assert_eq!(items.raw_get::<String>(2)?, "two");
        let payload: Buffer = table.raw_get("payload")?;
        assert_eq!(payload.to_vec(), [1, 2, 3]);
        Ok(())
    }

    #[test]
    fn inbound_map_hook_runs_before_generic_conversion() -> Result<()> {
        struct RefVisitor;

        impl InboundVisitor<Source> for RefVisitor {
            fn map(
                &mut self,
                entries: &[(InboundMapKey<'_>, &Source)],
                _lua: &Luau,
                path: &ValuePath,
            ) -> ValueVisitResult<BoundaryAction<Value>> {
                if entries.len() == 1
                    && let (InboundMapKey::String("$ref"), Source::Text(reference)) = entries[0]
                {
                    return Ok(BoundaryAction::Replace(Value::String(
                        _lua.create_string(format!("ref:{path}:{reference}"))
                            .map_err(|error| ValueVisitError::luau(path, error))?,
                    )));
                }
                Ok(BoundaryAction::Descend)
            }
        }

        let lua = Luau::new();
        let source = Source::Map(vec![(
            InboundKey::String("$ref".to_string()),
            Source::Text("handle".to_string()),
        )]);

        let value = inbound_to_luau(&lua, &source, &mut RefVisitor).expect("map hook should replace value");
        let Value::String(value) = value else {
            panic!("expected string");
        };
        assert_eq!(value.to_str()?, "ref:value:handle");
        Ok(())
    }

    #[test]
    fn inbound_reports_unsupported_key_path() {
        let lua = Luau::new();
        let source = Source::Map(vec![(InboundKey::Unsupported("integer"), Source::Nil)]);

        let error = inbound_to_luau(&lua, &source, &mut DefaultInboundVisitor)
            .expect_err("unsupported key should fail");

        assert!(matches!(error, ValueVisitError::UnsupportedTableKey { .. }));
        assert_eq!(error.path().to_string(), "value");
    }
}
