//! Outbound Luau value traversal.

use std::{
    collections::{BTreeMap, HashSet},
    os::raw::c_void,
};

use super::{ValuePath, ValueVisitError, ValueVisitResult};
use crate::{
    AnyUserData, Buffer, Error, Function, Integer, LightUserData, LuauString, Number, Table,
    Thread, Value, Vector,
};

pub(super) const MAX_VISIT_DEPTH: usize = 128;

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
    fn table(
        &mut self,
        _table: &Table,
        _path: &ValuePath,
    ) -> ValueVisitResult<BoundaryAction<Self::Output>> {
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
    fn array(
        &mut self,
        values: Vec<Self::Output>,
        path: &ValuePath,
    ) -> ValueVisitResult<Self::Output>;

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
pub fn visit_luau_value<V: OutboundVisitor>(
    value: &Value,
    visitor: &mut V,
) -> ValueVisitResult<V::Output> {
    visit_luau_value_at_path(value, ValuePath::value(), visitor)
}

/// Visits a Luau value with a caller-supplied root path.
pub fn visit_luau_value_at_path<V: OutboundVisitor>(
    value: &Value,
    path: impl Into<ValuePath>,
    visitor: &mut V,
) -> ValueVisitResult<V::Output> {
    let path = path.into();
    let mut active_tables = ActiveTables::new();
    visit_luau_value_inner(value, visitor, &path, &mut active_tables)
}

/// Recursive outbound traversal entrypoint.
fn visit_luau_value_inner<V: OutboundVisitor>(
    value: &Value,
    visitor: &mut V,
    path: &ValuePath,
    active_tables: &mut ActiveTables,
) -> ValueVisitResult<V::Output> {
    if path.depth() > MAX_VISIT_DEPTH {
        return Err(ValueVisitError::DepthLimit {
            path: path.clone(),
            max_depth: MAX_VISIT_DEPTH,
        });
    }

    match value {
        Value::Nil => visitor.nil(path),
        Value::Boolean(value) => visitor.boolean(*value, path),
        Value::Integer(value) => visitor.integer(*value, path),
        Value::Number(value) if value.is_finite() => visitor.number(*value, path),
        Value::Number(value) => {
            visitor.unsupported(UnsupportedOutboundValue::NonFiniteNumber(*value), path)
        }
        Value::Vector(value) => visitor.unsupported(UnsupportedOutboundValue::Vector(value), path),
        Value::String(value) => visitor.string(value, path),
        Value::Table(table) => visit_table(table, visitor, path, active_tables),
        Value::Function(value) => visit_host_value(HostValue::Function(value), visitor, path),
        Value::Thread(value) => visit_host_value(HostValue::Thread(value), visitor, path),
        Value::UserData(value) => visit_host_value(HostValue::UserData(value), visitor, path),
        Value::LightUserData(value) => {
            visit_host_value(HostValue::LightUserData(value), visitor, path)
        }
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
    active_tables: &mut ActiveTables,
) -> ValueVisitResult<V::Output> {
    match visitor.table(table, path)? {
        BoundaryAction::Replace(value) => return Ok(value),
        BoundaryAction::Descend => {}
    }

    active_tables.with_table(table, path, |active_tables| {
        visit_table_shape(table, visitor, path, active_tables)
    })
}

/// Visits a Luau table after determining whether it is an array or map.
fn visit_table_shape<V: OutboundVisitor>(
    table: &Table,
    visitor: &mut V,
    path: &ValuePath,
    active_tables: &mut ActiveTables,
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

/// Generic Luau table shape after boundary classification.
enum TableShape {
    /// Dense one-based array values.
    Array(Vec<(usize, Value)>),
    /// String-keyed map values.
    Map(Vec<(String, Value)>),
}

/// Active table stack for outbound cycle detection.
struct ActiveTables {
    pointers: HashSet<*const c_void>,
}

impl ActiveTables {
    fn new() -> Self {
        Self {
            pointers: HashSet::new(),
        }
    }

    fn with_table<T>(
        &mut self,
        table: &Table,
        path: &ValuePath,
        visit: impl FnOnce(&mut Self) -> ValueVisitResult<T>,
    ) -> ValueVisitResult<T> {
        let pointer = table.to_pointer();
        if !self.pointers.insert(pointer) {
            return Err(ValueVisitError::Cycle { path: path.clone() });
        }

        let result = visit(self);
        self.pointers.remove(&pointer);
        result
    }
}
