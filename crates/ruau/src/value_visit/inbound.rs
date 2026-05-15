//! Inbound host value conversion.

use super::{
    ValuePath, ValueVisitError, ValueVisitResult,
    outbound::{BoundaryAction, MAX_VISIT_DEPTH},
};
use crate::{Integer, Luau, Number, Value};

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
    if path.depth() > MAX_VISIT_DEPTH {
        return Err(ValueVisitError::DepthLimit {
            path,
            max_depth: MAX_VISIT_DEPTH,
        });
    }

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
        InboundKind::Unsupported(type_name) => {
            Err(ValueVisitError::UnsupportedValue { path, type_name })
        }
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
