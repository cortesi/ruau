use serde::{Serialize, ser};

use super::{
    BridgeError, RetainedTableSchema, Segment, attach_json_array_marker, clear_sequence_stale,
    integer_value, key_segment, scoped_string_key,
};
use crate::{
    DEFAULT_MAX_VALUE_MARSHAL_DEPTH, KeyHandle,
    scope::{Scope, ScopedValue, Table},
};

pub(super) struct ValueSerializer<'a, 's> {
    pub(super) scope: &'a Scope<'s>,
    pub(super) depth: usize,
}

/// Creates a table at `depth`, enforcing the marshal depth cap.
pub(super) fn new_table<'s>(scope: &Scope<'s>, depth: usize) -> Result<Table<'s>, BridgeError> {
    if depth >= DEFAULT_MAX_VALUE_MARSHAL_DEPTH {
        return Err(BridgeError::depth());
    }
    scope.create_table().map_err(BridgeError::from)
}

/// Wraps an already-serialized variant payload as `{ variant = payload }`.
fn wrap_variant<'s>(
    scope: &Scope<'s>,
    depth: usize,
    variant: &'static str,
    payload: ScopedValue<'s>,
) -> Result<ScopedValue<'s>, BridgeError> {
    let table = new_table(scope, depth)?;
    table
        .set(scope, variant, payload)
        .map_err(|error| BridgeError::from(error).at(Segment::Field(variant.to_owned())))?;
    Ok(ScopedValue::Table(table))
}

impl<'a, 's> ser::Serializer for ValueSerializer<'a, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;
    type SerializeSeq = SeqSerializer<'a, 's>;
    type SerializeTuple = SeqSerializer<'a, 's>;
    type SerializeTupleStruct = SeqSerializer<'a, 's>;
    type SerializeTupleVariant = VariantSeqSerializer<'a, 's>;
    type SerializeMap = MapSerializer<'a, 's>;
    type SerializeStruct = StructSerializer<'a, 's>;
    type SerializeStructVariant = VariantStructSerializer<'a, 's>;

    fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::Boolean(v))
    }

    fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
        Ok(integer_value(v))
    }

    fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
        i64::try_from(v)
            .map(integer_value)
            .map_err(|_| BridgeError::new("integer out of range for Lua: u64"))
    }

    fn serialize_f32(self, v: f32) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::Number(f64::from(v)))
    }

    fn serialize_f64(self, v: f64) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::Number(v))
    }

    fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
        let mut buf = [0u8; 4];
        self.serialize_str(v.encode_utf8(&mut buf))
    }

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        self.serialize_bytes(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::String(
            self.scope.create_string(v).map_err(BridgeError::from)?,
        ))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::Nil)
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::Nil)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::Nil)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        let payload = value
            .serialize(ValueSerializer {
                scope: self.scope,
                depth: self.depth + 1,
            })
            .map_err(|error| error.at(Segment::Field(variant.to_owned())))?;
        wrap_variant(self.scope, self.depth, variant, payload)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(SeqSerializer {
            scope: self.scope,
            table: new_table(self.scope, self.depth)?,
            index: 0,
            depth: self.depth,
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        // The `{ variant = ... }` wrapper sits at `depth`; its payload array
        // is one level deeper.
        if self.depth >= DEFAULT_MAX_VALUE_MARSHAL_DEPTH {
            return Err(BridgeError::depth());
        }
        Ok(VariantSeqSerializer {
            variant,
            inner: SeqSerializer {
                scope: self.scope,
                table: new_table(self.scope, self.depth + 1)?,
                index: 0,
                depth: self.depth + 1,
            },
        })
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(MapSerializer {
            scope: self.scope,
            table: new_table(self.scope, self.depth)?,
            pending: None,
            depth: self.depth,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(StructSerializer {
            scope: self.scope,
            table: new_table(self.scope, self.depth)?,
            depth: self.depth,
        })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        if self.depth >= DEFAULT_MAX_VALUE_MARSHAL_DEPTH {
            return Err(BridgeError::depth());
        }
        Ok(VariantStructSerializer {
            variant,
            inner: StructSerializer {
                scope: self.scope,
                table: new_table(self.scope, self.depth + 1)?,
                depth: self.depth + 1,
            },
        })
    }
}

/// Builds an array-shaped table: elements land under number keys `1..n` (the
/// table's array part, the shape `#t` and `Vec` conversions use).
pub(super) struct SeqSerializer<'a, 's> {
    scope: &'a Scope<'s>,
    table: Table<'s>,
    index: u64,
    depth: usize,
}

impl<'s> SeqSerializer<'_, 's> {
    fn push<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), BridgeError> {
        self.index += 1;
        let segment = Segment::Index(self.index);
        let value = value
            .serialize(ValueSerializer {
                scope: self.scope,
                depth: self.depth + 1,
            })
            .map_err(|error| error.at(segment.clone()))?;
        self.table
            .set(self.scope, self.index as f64, value)
            .map_err(|error| BridgeError::from(error).at(segment))
    }

    fn finish(self) -> Result<ScopedValue<'s>, BridgeError> {
        attach_json_array_marker(self.scope, self.table)?;
        Ok(ScopedValue::Table(self.table))
    }
}

impl<'s> ser::SerializeSeq for SeqSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.finish()
    }
}

impl<'s> ser::SerializeTuple for SeqSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.finish()
    }
}

impl<'s> ser::SerializeTupleStruct for SeqSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.finish()
    }
}

/// Builds `{ variant = { payload... } }` for a tuple variant.
pub(super) struct VariantSeqSerializer<'a, 's> {
    variant: &'static str,
    inner: SeqSerializer<'a, 's>,
}

impl<'s> ser::SerializeTupleVariant for VariantSeqSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        let variant = self.variant;
        self.inner
            .push(value)
            .map_err(|error| error.at(Segment::Field(variant.to_owned())))
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        let scope = self.inner.scope;
        let depth = self.inner.depth - 1;
        wrap_variant(scope, depth, self.variant, self.inner.finish()?)
    }
}

/// Builds a map-shaped table; keys may be any serializable scalar Lua accepts
/// as a table key (`nil` and NaN keys are rejected by the table itself).
pub(super) struct MapSerializer<'a, 's> {
    scope: &'a Scope<'s>,
    table: Table<'s>,
    pending: Option<(ScopedValue<'s>, Segment)>,
    depth: usize,
}

impl<'s> ser::SerializeMap for MapSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Self::Error> {
        let key = key
            .serialize(ValueSerializer {
                scope: self.scope,
                depth: self.depth + 1,
            })
            .map_err(|error| error.at(Segment::Key("<map key>".to_owned())))?;
        self.pending = Some((key, key_segment(self.scope, key)));
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        let (key, segment) = self
            .pending
            .take()
            .ok_or_else(|| BridgeError::new("serialize_value called before serialize_key"))?;
        let value = value
            .serialize(ValueSerializer {
                scope: self.scope,
                depth: self.depth + 1,
            })
            .map_err(|error| error.at(segment.clone()))?;
        self.table
            .set(self.scope, key, value)
            .map_err(|error| BridgeError::from(error).at(segment))
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::Table(self.table))
    }
}

/// Builds a struct-shaped table with string field keys.
pub(super) struct StructSerializer<'a, 's> {
    scope: &'a Scope<'s>,
    table: Table<'s>,
    depth: usize,
}

impl<'s> StructSerializer<'_, 's> {
    fn put<T: Serialize + ?Sized>(&self, key: &'static str, value: &T) -> Result<(), BridgeError> {
        let value = value
            .serialize(ValueSerializer {
                scope: self.scope,
                depth: self.depth + 1,
            })
            .map_err(|error| error.at(Segment::Field(key.to_owned())))?;
        self.table
            .set(self.scope, key, value)
            .map_err(|error| BridgeError::from(error).at(Segment::Field(key.to_owned())))
    }
}

impl<'s> ser::SerializeStruct for StructSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.put(key, value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::Table(self.table))
    }
}

/// Builds `{ variant = { fields... } }` for a struct variant.
pub(super) struct VariantStructSerializer<'a, 's> {
    variant: &'static str,
    inner: StructSerializer<'a, 's>,
}

impl<'s> ser::SerializeStructVariant for VariantStructSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        let variant = self.variant;
        self.inner
            .put(key, value)
            .map_err(|error| error.at(Segment::Field(variant.to_owned())))
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        let scope = self.inner.scope;
        let depth = self.inner.depth - 1;
        wrap_variant(
            scope,
            depth,
            self.variant,
            ScopedValue::Table(self.inner.table),
        )
    }
}

// ---------------------------------------------------------------------------
// Retained-table serializer
// ---------------------------------------------------------------------------

/// Serializer that shares the scalar encoding with [`ValueSerializer`] but
/// writes table-shaped values into an existing retained table when supplied.
pub(super) struct RetainedValueSerializer<'a, 's> {
    pub(super) scope: &'a Scope<'s>,
    pub(super) schema: &'a mut RetainedTableSchema,
    pub(super) node: usize,
    pub(super) depth: usize,
    pub(super) target: Option<Table<'s>>,
}

impl<'a, 's> RetainedValueSerializer<'a, 's> {
    fn child_target(&self, value: ScopedValue<'s>) -> Option<Table<'s>> {
        match value {
            ScopedValue::Table(table) => Some(table),
            _ => None,
        }
    }

    fn table(&self) -> Result<Table<'s>, BridgeError> {
        if self.depth >= DEFAULT_MAX_VALUE_MARSHAL_DEPTH {
            return Err(BridgeError::depth());
        }
        match self.target {
            Some(table) => Ok(table),
            None => new_table(self.scope, self.depth),
        }
    }

    fn fresh(self) -> ValueSerializer<'a, 's> {
        ValueSerializer {
            scope: self.scope,
            depth: self.depth,
        }
    }
}

impl<'a, 's> ser::Serializer for RetainedValueSerializer<'a, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;
    type SerializeSeq = RetainedSeqSerializer<'a, 's>;
    type SerializeTuple = RetainedSeqSerializer<'a, 's>;
    type SerializeTupleStruct = RetainedSeqSerializer<'a, 's>;
    type SerializeTupleVariant = RetainedVariantSeqSerializer<'a, 's>;
    type SerializeMap = RetainedMapSerializer<'a, 's>;
    type SerializeStruct = RetainedStructSerializer<'a, 's>;
    type SerializeStructVariant = RetainedVariantStructSerializer<'a, 's>;

    fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_bool(v)
    }

    fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_i8(v)
    }

    fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_i16(v)
    }

    fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_i32(v)
    }

    fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_i64(v)
    }

    fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_u8(v)
    }

    fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_u16(v)
    }

    fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_u32(v)
    }

    fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_u64(v)
    }

    fn serialize_f32(self, v: f32) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_f32(v)
    }

    fn serialize_f64(self, v: f64) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_f64(v)
    }

    fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_char(v)
    }

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_str(v)
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_bytes(v)
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_none()
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_unit()
    }

    fn serialize_unit_struct(self, name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.fresh().serialize_unit_struct(name)
    }

    fn serialize_unit_variant(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.fresh()
            .serialize_unit_variant(name, variant_index, variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        let table = self.table()?;
        let (key, child_node) = self.schema.keyed_child(self.scope, self.node, variant)?;
        let existing = if self.schema.should_probe_node(child_node) {
            table.get_keyed::<ScopedValue<'_>>(self.scope, &key)?
        } else {
            ScopedValue::Nil
        };
        let target = self.child_target(existing);
        let payload = value
            .serialize(RetainedValueSerializer {
                scope: self.scope,
                schema: self.schema,
                node: child_node,
                depth: self.depth + 1,
                target,
            })
            .map_err(|error| error.at(Segment::Field(variant.to_owned())))?;
        self.schema.remember_node_shape(child_node, payload);
        table
            .set_keyed(self.scope, &key, payload)
            .map_err(|error| BridgeError::from(error).at(Segment::Field(variant.to_owned())))?;
        self.schema
            .finish_keyed_node(self.scope, table, self.node, vec![key], true)?;
        Ok(ScopedValue::Table(table))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        let depth = self.depth;
        let table = self.table()?;
        Ok(RetainedSeqSerializer {
            scope: self.scope,
            schema: self.schema,
            node: self.node,
            table,
            index: 0,
            depth,
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        let depth = self.depth;
        let wrapper = self.table()?;
        let (key, variant_node) = self.schema.keyed_child(self.scope, self.node, variant)?;
        let existing = if self.schema.should_probe_node(variant_node) {
            wrapper.get_keyed::<ScopedValue<'_>>(self.scope, &key)?
        } else {
            ScopedValue::Nil
        };
        let inner = match existing {
            ScopedValue::Table(table) => table,
            _ => new_table(self.scope, depth + 1)?,
        };
        Ok(RetainedVariantSeqSerializer {
            variant,
            node: self.node,
            wrapper,
            inner: RetainedSeqSerializer {
                scope: self.scope,
                schema: self.schema,
                node: variant_node,
                table: inner,
                index: 0,
                depth: depth + 1,
            },
        })
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        let depth = self.depth;
        let table = self.table()?;
        Ok(RetainedMapSerializer {
            scope: self.scope,
            schema: self.schema,
            node: self.node,
            table,
            pending: None,
            visited: Vec::new(),
            depth,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        let depth = self.depth;
        let table = self.table()?;
        Ok(RetainedStructSerializer {
            scope: self.scope,
            schema: self.schema,
            node: self.node,
            table,
            visited: Vec::new(),
            depth,
        })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        let depth = self.depth;
        let wrapper = self.table()?;
        let (key, variant_node) = self.schema.keyed_child(self.scope, self.node, variant)?;
        let existing = if self.schema.should_probe_node(variant_node) {
            wrapper.get_keyed::<ScopedValue<'_>>(self.scope, &key)?
        } else {
            ScopedValue::Nil
        };
        let inner = match existing {
            ScopedValue::Table(table) => table,
            _ => new_table(self.scope, depth + 1)?,
        };
        Ok(RetainedVariantStructSerializer {
            variant,
            node: self.node,
            wrapper,
            inner: RetainedStructSerializer {
                scope: self.scope,
                schema: self.schema,
                node: variant_node,
                table: inner,
                visited: Vec::new(),
                depth: depth + 1,
            },
        })
    }
}

pub(super) struct RetainedSeqSerializer<'a, 's> {
    scope: &'a Scope<'s>,
    schema: &'a mut RetainedTableSchema,
    node: usize,
    table: Table<'s>,
    index: u64,
    depth: usize,
}

impl<'s> RetainedSeqSerializer<'_, 's> {
    fn push<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), BridgeError> {
        self.index += 1;
        let segment = Segment::Index(self.index);
        let child_node = self.schema.sequence_child_node(self.node);
        let existing = if self.schema.should_probe_node(child_node) {
            self.table
                .get::<_, ScopedValue<'_>>(self.scope, self.index as f64)?
        } else {
            ScopedValue::Nil
        };
        let target = match existing {
            ScopedValue::Table(table) => Some(table),
            _ => None,
        };
        let value = value
            .serialize(RetainedValueSerializer {
                scope: self.scope,
                schema: self.schema,
                node: child_node,
                depth: self.depth + 1,
                target,
            })
            .map_err(|error| error.at(segment.clone()))?;
        self.schema.remember_node_shape(child_node, value);
        self.table
            .set(self.scope, self.index as f64, value)
            .map_err(|error| BridgeError::from(error).at(segment))
    }

    fn finish(self) -> Result<ScopedValue<'s>, BridgeError> {
        clear_sequence_stale(self.scope, self.table, self.index)?;
        attach_json_array_marker(self.scope, self.table)?;
        Ok(ScopedValue::Table(self.table))
    }
}

impl<'s> ser::SerializeSeq for RetainedSeqSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.finish()
    }
}

impl<'s> ser::SerializeTuple for RetainedSeqSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.finish()
    }
}

impl<'s> ser::SerializeTupleStruct for RetainedSeqSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.finish()
    }
}

pub(super) struct RetainedMapKey<'s> {
    value: ScopedValue<'s>,
    segment: Segment,
    keyed: Option<KeyHandle>,
    child_node: usize,
}

pub(super) struct RetainedMapSerializer<'a, 's> {
    scope: &'a Scope<'s>,
    schema: &'a mut RetainedTableSchema,
    node: usize,
    table: Table<'s>,
    pending: Option<RetainedMapKey<'s>>,
    visited: Vec<KeyHandle>,
    depth: usize,
}

impl<'s> ser::SerializeMap for RetainedMapSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Self::Error> {
        let value = key
            .serialize(ValueSerializer {
                scope: self.scope,
                depth: self.depth + 1,
            })
            .map_err(|error| error.at(Segment::Key("<map key>".to_owned())))?;
        let (keyed, child_node) = match scoped_string_key(self.scope, value)? {
            Some(text) => {
                let (handle, child_node) = self.schema.keyed_child(self.scope, self.node, &text)?;
                (Some(handle), child_node)
            }
            None => (None, self.schema.non_string_child_node(self.node)),
        };
        self.pending = Some(RetainedMapKey {
            value,
            segment: key_segment(self.scope, value),
            keyed,
            child_node,
        });
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        let key = self
            .pending
            .take()
            .ok_or_else(|| BridgeError::new("serialize_value called before serialize_key"))?;
        let RetainedMapKey {
            value: key_value,
            segment,
            keyed,
            child_node,
        } = key;
        let existing = if self.schema.should_probe_node(child_node) {
            match &keyed {
                Some(handle) => self
                    .table
                    .get_keyed::<ScopedValue<'_>>(self.scope, handle)?,
                None => self
                    .table
                    .get::<_, ScopedValue<'_>>(self.scope, key_value)?,
            }
        } else {
            ScopedValue::Nil
        };
        let target = match existing {
            ScopedValue::Table(table) => Some(table),
            _ => None,
        };
        let encoded = value
            .serialize(RetainedValueSerializer {
                scope: self.scope,
                schema: self.schema,
                node: child_node,
                depth: self.depth + 1,
                target,
            })
            .map_err(|error| error.at(segment.clone()))?;
        self.schema.remember_node_shape(child_node, encoded);
        match keyed {
            Some(handle) => {
                self.table
                    .set_keyed(self.scope, &handle, encoded)
                    .map_err(|error| BridgeError::from(error).at(segment))?;
                self.visited.push(handle);
            }
            None => self
                .table
                .set(self.scope, key_value, encoded)
                .map_err(|error| BridgeError::from(error).at(segment))?,
        }
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.schema
            .finish_keyed_node(self.scope, self.table, self.node, self.visited, false)?;
        Ok(ScopedValue::Table(self.table))
    }
}

pub(super) struct RetainedStructSerializer<'a, 's> {
    scope: &'a Scope<'s>,
    schema: &'a mut RetainedTableSchema,
    node: usize,
    table: Table<'s>,
    visited: Vec<KeyHandle>,
    depth: usize,
}

impl<'s> RetainedStructSerializer<'_, 's> {
    fn put<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), BridgeError> {
        let (handle, child_node) = self.schema.keyed_child(self.scope, self.node, key)?;
        let existing = if self.schema.should_probe_node(child_node) {
            self.table
                .get_keyed::<ScopedValue<'_>>(self.scope, &handle)?
        } else {
            ScopedValue::Nil
        };
        let target = match existing {
            ScopedValue::Table(table) => Some(table),
            _ => None,
        };
        let encoded = value
            .serialize(RetainedValueSerializer {
                scope: self.scope,
                schema: self.schema,
                node: child_node,
                depth: self.depth + 1,
                target,
            })
            .map_err(|error| error.at(Segment::Field(key.to_owned())))?;
        self.schema.remember_node_shape(child_node, encoded);
        self.table
            .set_keyed(self.scope, &handle, encoded)
            .map_err(|error| BridgeError::from(error).at(Segment::Field(key.to_owned())))?;
        self.visited.push(handle);
        Ok(())
    }

    fn clear_field(&mut self, key: &'static str) -> Result<(), BridgeError> {
        let (handle, child_node) = self.schema.keyed_child(self.scope, self.node, key)?;
        self.schema
            .remember_node_shape(child_node, ScopedValue::Nil);
        self.table
            .set_keyed(self.scope, &handle, ())
            .map_err(|error| BridgeError::from(error).at(Segment::Field(key.to_owned())))?;
        self.visited.push(handle);
        Ok(())
    }

    fn finish(self) -> Result<Table<'s>, BridgeError> {
        self.schema
            .finish_keyed_node(self.scope, self.table, self.node, self.visited, true)?;
        Ok(self.table)
    }
}

impl<'s> ser::SerializeStruct for RetainedStructSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.put(key, value)
    }

    fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
        self.clear_field(key)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(ScopedValue::Table(self.finish()?))
    }
}

pub(super) struct RetainedVariantSeqSerializer<'a, 's> {
    variant: &'static str,
    node: usize,
    wrapper: Table<'s>,
    inner: RetainedSeqSerializer<'a, 's>,
}

impl<'s> ser::SerializeTupleVariant for RetainedVariantSeqSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.inner.push(value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        let RetainedVariantSeqSerializer {
            variant,
            node,
            wrapper,
            inner,
        } = self;
        let RetainedSeqSerializer {
            scope,
            schema,
            node: inner_node,
            table,
            index,
            ..
        } = inner;
        clear_sequence_stale(scope, table, index)?;
        attach_json_array_marker(scope, table)?;
        let key = schema.key(scope, variant)?;
        schema.remember_node_shape(inner_node, ScopedValue::Table(table));
        wrapper
            .set_keyed(scope, &key, ScopedValue::Table(table))
            .map_err(|error| BridgeError::from(error).at(Segment::Field(variant.to_owned())))?;
        schema.finish_keyed_node(scope, wrapper, node, vec![key], true)?;
        Ok(ScopedValue::Table(wrapper))
    }
}

pub(super) struct RetainedVariantStructSerializer<'a, 's> {
    variant: &'static str,
    node: usize,
    wrapper: Table<'s>,
    inner: RetainedStructSerializer<'a, 's>,
}

impl<'s> ser::SerializeStructVariant for RetainedVariantStructSerializer<'_, 's> {
    type Ok = ScopedValue<'s>;
    type Error = BridgeError;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.inner.put(key, value)
    }

    fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
        self.inner.clear_field(key)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        let RetainedVariantStructSerializer {
            variant,
            node,
            wrapper,
            inner,
        } = self;
        let RetainedStructSerializer {
            scope,
            schema,
            node: inner_node,
            table,
            visited,
            ..
        } = inner;
        schema.finish_keyed_node(scope, table, inner_node, visited, true)?;
        let key = schema.key(scope, variant)?;
        schema.remember_node_shape(inner_node, ScopedValue::Table(table));
        wrapper
            .set_keyed(scope, &key, ScopedValue::Table(table))
            .map_err(|error| BridgeError::from(error).at(Segment::Field(variant.to_owned())))?;
        schema.finish_keyed_node(scope, wrapper, node, vec![key], true)?;
        Ok(ScopedValue::Table(wrapper))
    }
}
