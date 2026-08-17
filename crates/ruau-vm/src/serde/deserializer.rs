use std::{cell::RefCell, rc::Rc};

use serde::de::{self, IntoDeserializer};

use super::{
    BridgeError, Segment, TableShape, classify_table, exact_integer, has_json_array_marker,
    key_segment, type_error,
};
use crate::{
    Limits, ValueMarshalLimits,
    scope::{Scope, ScopedValue, Table},
};

pub(super) type SharedDeserializeBudget = Rc<RefCell<ValueDeserializeBudget>>;

pub(super) struct ValueDeserializeBudget {
    limits: ValueMarshalLimits,
    nodes: usize,
    table_entries: usize,
}

impl ValueDeserializeBudget {
    pub(super) fn shared_default() -> SharedDeserializeBudget {
        Rc::new(RefCell::new(Self::new(ValueMarshalLimits::from(
            Limits::unlimited().effective(),
        ))))
    }

    #[cfg(any())]
    pub(super) fn shared_with_limits(limits: ValueMarshalLimits) -> SharedDeserializeBudget {
        Rc::new(RefCell::new(Self::new(limits)))
    }

    fn new(limits: ValueMarshalLimits) -> Self {
        Self {
            limits,
            nodes: 0,
            table_entries: 0,
        }
    }

    pub(super) fn max_depth(&self) -> usize {
        self.limits.max_depth
    }

    pub(super) fn bump_node(&mut self) -> Result<(), BridgeError> {
        self.bump_nodes(1)
    }

    fn bump_nodes(&mut self, count: usize) -> Result<(), BridgeError> {
        self.nodes = self
            .nodes
            .checked_add(count)
            .ok_or_else(|| BridgeError::new("value count overflowed while deserializing"))?;
        if self.nodes > self.limits.max_nodes {
            return Err(BridgeError::new(format!(
                "value count exceeds marshal cap {}",
                self.limits.max_nodes
            )));
        }
        Ok(())
    }

    pub(super) fn charge_table_entries(&mut self, count: usize) -> Result<(), BridgeError> {
        self.table_entries = self
            .table_entries
            .checked_add(count)
            .ok_or_else(|| BridgeError::new("table entry count overflowed while deserializing"))?;
        if self.table_entries > self.limits.max_table_entries {
            return Err(BridgeError::new(format!(
                "table entries exceed marshal cap {}",
                self.limits.max_table_entries
            )));
        }
        Ok(())
    }

    pub(super) fn charge_string_bytes(&self, len: usize) -> Result<(), BridgeError> {
        if len > self.limits.max_string_bytes {
            return Err(BridgeError::new(format!(
                "string is {len} bytes, over the {}-byte marshal cap",
                self.limits.max_string_bytes
            )));
        }
        Ok(())
    }

    fn charge_buffer_bytes(&self, len: usize) -> Result<(), BridgeError> {
        if len > self.limits.max_buffer_bytes {
            return Err(BridgeError::new(format!(
                "buffer is {len} bytes, over the {}-byte marshal cap",
                self.limits.max_buffer_bytes
            )));
        }
        Ok(())
    }
}

pub(super) struct ValueDeserializer<'a, 's> {
    pub(super) scope: &'a Scope<'s>,
    pub(super) value: ScopedValue<'s>,
    pub(super) depth: usize,
    pub(super) budget: SharedDeserializeBudget,
}

impl<'a, 's> ValueDeserializer<'a, 's> {
    pub(super) fn new(scope: &'a Scope<'s>, value: ScopedValue<'s>) -> Self {
        Self::with_budget(scope, value, 0, ValueDeserializeBudget::shared_default())
    }

    pub(super) fn with_budget(
        scope: &'a Scope<'s>,
        value: ScopedValue<'s>,
        depth: usize,
        budget: SharedDeserializeBudget,
    ) -> Self {
        Self {
            scope,
            value,
            depth,
            budget,
        }
    }

    fn child(&self, value: ScopedValue<'s>) -> Self {
        Self::with_budget(self.scope, value, self.depth + 1, Rc::clone(&self.budget))
    }

    pub(super) fn charge_node(&self) -> Result<(), BridgeError> {
        self.budget.borrow_mut().bump_node()
    }

    pub(super) fn charge_string_bytes(&self, len: usize) -> Result<(), BridgeError> {
        self.budget.borrow().charge_string_bytes(len)
    }

    pub(super) fn charge_buffer_bytes(&self, len: usize) -> Result<(), BridgeError> {
        self.budget.borrow().charge_buffer_bytes(len)
    }

    fn check_depth(&self) -> Result<(), BridgeError> {
        let max_depth = self.budget.borrow().max_depth();
        if self.depth >= max_depth {
            return Err(BridgeError::depth_limit(max_depth));
        }
        Ok(())
    }

    /// Snapshots a table's pairs, enforcing the depth cap before recursing.
    fn table_pairs(
        &self,
        table: Table<'s>,
    ) -> Result<Vec<(ScopedValue<'s>, ScopedValue<'s>)>, BridgeError> {
        self.check_depth()?;
        let pair_count = table.pair_count(self.scope).map_err(BridgeError::from)?;
        self.budget.borrow_mut().charge_table_entries(pair_count)?;
        table.pairs(self.scope).map_err(BridgeError::from)
    }

    /// Reads the value as Lua's 64-bit integer: a native integer, or a number
    /// that is exactly integral and in range.
    fn lua_integer(&self) -> Result<i64, BridgeError> {
        match self.value {
            ScopedValue::Integer(value) => Ok(value),
            ScopedValue::Number(value) => match exact_integer(value) {
                Some(value) => Ok(value),
                None if value.fract() == 0.0 => Err(BridgeError::new(format!(
                    "number {value} is out of range for a 64-bit integer"
                ))),
                None => Err(BridgeError::new(format!(
                    "expected integer, got non-integral number {value}"
                ))),
            },
            other => Err(type_error("integer", other)),
        }
    }

    pub(super) fn utf8_string(&self) -> Result<String, BridgeError> {
        match self.value {
            ScopedValue::String(handle) => {
                let len = self.scope.string_len(handle).map_err(BridgeError::from)?;
                self.charge_string_bytes(len)?;
                let bytes = self.scope.string_bytes(handle).map_err(BridgeError::from)?;
                String::from_utf8(bytes)
                    .map_err(|_| BridgeError::new("expected UTF-8 string, got non-UTF-8 bytes"))
            }
            other => Err(type_error("string", other)),
        }
    }

    fn byte_payload(&self) -> Result<Vec<u8>, BridgeError> {
        match self.value {
            ScopedValue::String(handle) => {
                let len = self.scope.string_len(handle).map_err(BridgeError::from)?;
                self.charge_string_bytes(len)?;
                self.scope.string_bytes(handle).map_err(BridgeError::from)
            }
            ScopedValue::Buffer(handle) => {
                let len = self.scope.buffer_len(handle).map_err(BridgeError::from)?;
                self.charge_buffer_bytes(len)?;
                self.scope.buffer_bytes(handle).map_err(BridgeError::from)
            }
            other => Err(type_error("string or buffer", other)),
        }
    }

    fn seq_access(self, items: Vec<ScopedValue<'s>>) -> BridgeSeqAccess<'a, 's> {
        BridgeSeqAccess {
            scope: self.scope,
            items: items.into_iter(),
            index: 0,
            depth: self.depth,
            budget: self.budget,
        }
    }

    fn map_access(self, pairs: Vec<(ScopedValue<'s>, ScopedValue<'s>)>) -> BridgeMapAccess<'a, 's> {
        BridgeMapAccess {
            scope: self.scope,
            pairs: pairs.into_iter(),
            pending: None,
            depth: self.depth,
            budget: self.budget,
        }
    }
}

macro_rules! deserialize_integer {
    ($($method:ident => $t:ty => $visit:ident,)+) => {
        $(
            fn $method<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
                self.charge_node()?;
                let value = self.lua_integer()?;
                let value = <$t>::try_from(value).map_err(|_| {
                    BridgeError::new(concat!("integer out of range for ", stringify!($t)))
                })?;
                visitor.$visit(value)
            }
        )+
    };
}

impl<'de, 's> de::Deserializer<'de> for ValueDeserializer<'_, 's> {
    type Error = BridgeError;

    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        match self.value {
            ScopedValue::Nil => visitor.visit_unit(),
            ScopedValue::Boolean(value) => visitor.visit_bool(value),
            ScopedValue::Integer(value) => visitor.visit_i64(value),
            // An exactly-integral number self-describes as an integer:
            // script-side number literals materialize as `Number`, and a
            // buffering decode (internally tagged and untagged enums,
            // `flatten`, `serde_json::Value`) applies no integral relaxation
            // of its own, so `{ dx = 1 }` must buffer as i64 to reach an i64
            // field.
            ScopedValue::Number(value) => match exact_integer(value) {
                Some(value) => visitor.visit_i64(value),
                None => visitor.visit_f64(value),
            },
            ScopedValue::String(_) => {
                // Text when it is text; raw bytes otherwise.
                match self.utf8_string() {
                    Ok(text) => visitor.visit_string(text),
                    Err(_) => visitor.visit_byte_buf(self.byte_payload()?),
                }
            }
            ScopedValue::Buffer(_) => visitor.visit_byte_buf(self.byte_payload()?),
            ScopedValue::Table(table) => {
                let marked_array = has_json_array_marker(self.scope, table)?;
                let pairs = self.table_pairs(table)?;
                match classify_table(pairs) {
                    TableShape::Empty if marked_array => {
                        visitor.visit_seq(self.seq_access(Vec::new()))
                    }
                    TableShape::Empty => visitor.visit_map(self.map_access(Vec::new())),
                    TableShape::Seq(items) => visitor.visit_seq(self.seq_access(items)),
                    TableShape::Map(_) if marked_array => Err(BridgeError::new(
                        "JSON array marker requires integer keys 1..n",
                    )),
                    TableShape::Map(pairs) => visitor.visit_map(self.map_access(pairs)),
                }
            }
            other => Err(BridgeError::new(format!(
                "cannot deserialize a Lua {} into a serde value",
                other.type_name()
            ))),
        }
    }

    fn deserialize_bool<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        match self.value {
            ScopedValue::Boolean(value) => visitor.visit_bool(value),
            other => Err(type_error("boolean", other)),
        }
    }

    deserialize_integer! {
        deserialize_i8 => i8 => visit_i8,
        deserialize_i16 => i16 => visit_i16,
        deserialize_i32 => i32 => visit_i32,
        deserialize_i64 => i64 => visit_i64,
        deserialize_u8 => u8 => visit_u8,
        deserialize_u16 => u16 => visit_u16,
        deserialize_u32 => u32 => visit_u32,
        deserialize_u64 => u64 => visit_u64,
    }

    fn deserialize_f32<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_f64(visitor)
    }

    fn deserialize_f64<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        match self.value {
            ScopedValue::Number(value) => visitor.visit_f64(value),
            #[expect(
                clippy::cast_precision_loss,
                reason = "widening an i64 toward f64 is the documented number relaxation"
            )]
            ScopedValue::Integer(value) => visitor.visit_f64(value as f64),
            other => Err(type_error("number", other)),
        }
    }

    fn deserialize_char<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        let text = self.utf8_string()?;
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => visitor.visit_char(c),
            _ => Err(BridgeError::new(format!(
                "expected a one-character string, got {} characters",
                text.chars().count()
            ))),
        }
    }

    fn deserialize_str<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        visitor.visit_string(self.utf8_string()?)
    }

    fn deserialize_string<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        visitor.visit_byte_buf(self.byte_payload()?)
    }

    fn deserialize_byte_buf<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        match self.value {
            ScopedValue::Nil => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        match self.value {
            ScopedValue::Nil => visitor.visit_unit(),
            other => Err(type_error("nil", other)),
        }
    }

    fn deserialize_unit_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        match self.value {
            ScopedValue::Table(table) => {
                let pairs = self.table_pairs(table)?;
                match classify_table(pairs) {
                    TableShape::Empty => visitor.visit_seq(self.seq_access(Vec::new())),
                    TableShape::Seq(items) => visitor.visit_seq(self.seq_access(items)),
                    TableShape::Map(_) => Err(BridgeError::new(
                        "expected an array table (integer keys 1..n), got a map-shaped table",
                    )),
                }
            }
            other => Err(type_error("table", other)),
        }
    }

    fn deserialize_tuple<V: de::Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        match self.value {
            ScopedValue::Table(table) => {
                let pairs = self.table_pairs(table)?;
                visitor.visit_map(self.map_access(pairs))
            }
            other => Err(type_error("table", other)),
        }
    }

    fn deserialize_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        match self.value {
            // A bare string is a unit variant.
            ScopedValue::String(_) => visitor.visit_enum(BridgeEnumAccess {
                scope: self.scope,
                variant: self.utf8_string()?,
                payload: None,
                depth: self.depth,
                budget: self.budget,
            }),
            ScopedValue::Table(table) => {
                let pairs = self.table_pairs(table)?;
                if pairs.len() != 1 {
                    return Err(BridgeError::new(format!(
                        "expected a single-pair table for an externally tagged enum variant, \
                         got {} entries",
                        pairs.len()
                    )));
                }
                let (key, payload) = pairs[0];
                let variant_key = self.child(key);
                variant_key.charge_node()?;
                let variant = variant_key
                    .utf8_string()
                    .map_err(|_| type_error("string variant key", key))?;
                visitor.visit_enum(BridgeEnumAccess {
                    scope: self.scope,
                    variant,
                    payload: Some(payload),
                    depth: self.depth,
                    budget: self.budget,
                })
            }
            other => Err(type_error("enum (string or single-pair table)", other)),
        }
    }

    fn deserialize_identifier<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.charge_node()?;
        // Anything — including heap values serde cannot represent — may be
        // skipped without error.
        visitor.visit_unit()
    }
}

/// Sequence access over a snapshot of an array-shaped table.
struct BridgeSeqAccess<'a, 's> {
    scope: &'a Scope<'s>,
    items: std::vec::IntoIter<ScopedValue<'s>>,
    index: u64,
    depth: usize,
    budget: SharedDeserializeBudget,
}

impl<'de, 's> de::SeqAccess<'de> for BridgeSeqAccess<'_, 's> {
    type Error = BridgeError;

    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        let Some(value) = self.items.next() else {
            return Ok(None);
        };
        self.index += 1;
        seed.deserialize(ValueDeserializer::with_budget(
            self.scope,
            value,
            self.depth + 1,
            Rc::clone(&self.budget),
        ))
        .map(Some)
        .map_err(|error| error.at(Segment::Index(self.index)))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

/// Map access over a snapshot of a table's pairs.
struct BridgeMapAccess<'a, 's> {
    scope: &'a Scope<'s>,
    pairs: std::vec::IntoIter<(ScopedValue<'s>, ScopedValue<'s>)>,
    pending: Option<(ScopedValue<'s>, Segment)>,
    depth: usize,
    budget: SharedDeserializeBudget,
}

impl<'de, 's> de::MapAccess<'de> for BridgeMapAccess<'_, 's> {
    type Error = BridgeError;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some((key, value)) = self.pairs.next() else {
            return Ok(None);
        };
        let segment = key_segment(self.scope, key);
        self.pending = Some((value, segment.clone()));
        seed.deserialize(ValueDeserializer::with_budget(
            self.scope,
            key,
            self.depth + 1,
            Rc::clone(&self.budget),
        ))
        .map(Some)
        .map_err(|error| error.at(segment))
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let (value, segment) = self
            .pending
            .take()
            .ok_or_else(|| BridgeError::new("next_value_seed called before next_key_seed"))?;
        seed.deserialize(ValueDeserializer::with_budget(
            self.scope,
            value,
            self.depth + 1,
            Rc::clone(&self.budget),
        ))
        .map_err(|error| error.at(segment))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.pairs.len())
    }
}

/// Enum access for the externally tagged shape: a bare variant string, or a
/// single-pair `{ variant = payload }` table.
struct BridgeEnumAccess<'a, 's> {
    scope: &'a Scope<'s>,
    variant: String,
    payload: Option<ScopedValue<'s>>,
    depth: usize,
    budget: SharedDeserializeBudget,
}

impl<'de, 'a, 's> de::EnumAccess<'de> for BridgeEnumAccess<'a, 's> {
    type Error = BridgeError;
    type Variant = BridgeVariantAccess<'a, 's>;

    fn variant_seed<V: de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        let variant_name = self.variant.clone();
        let deserializer: de::value::StringDeserializer<BridgeError> =
            self.variant.into_deserializer();
        let variant = seed.deserialize(deserializer)?;
        Ok((
            variant,
            BridgeVariantAccess {
                scope: self.scope,
                variant: variant_name,
                payload: self.payload,
                depth: self.depth,
                budget: self.budget,
            },
        ))
    }
}

/// Variant payload access for [`BridgeEnumAccess`].
struct BridgeVariantAccess<'a, 's> {
    scope: &'a Scope<'s>,
    variant: String,
    payload: Option<ScopedValue<'s>>,
    depth: usize,
    budget: SharedDeserializeBudget,
}

impl<'a, 's> BridgeVariantAccess<'a, 's> {
    fn payload_deserializer(self) -> Result<ValueDeserializer<'a, 's>, BridgeError> {
        let variant = self.variant;
        let payload = self.payload.ok_or_else(|| {
            BridgeError::new(format!(
                "variant `{variant}` is a bare string and carries no payload"
            ))
        })?;
        Ok(ValueDeserializer::with_budget(
            self.scope,
            payload,
            self.depth + 1,
            self.budget,
        ))
    }
}

impl<'de> de::VariantAccess<'de> for BridgeVariantAccess<'_, '_> {
    type Error = BridgeError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        match self.payload {
            None | Some(ScopedValue::Nil) => Ok(()),
            Some(other) => Err(type_error(
                &format!("no payload for unit variant `{}`", self.variant),
                other,
            )),
        }
    }

    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        let segment = Segment::Field(self.variant.clone());
        seed.deserialize(self.payload_deserializer()?)
            .map_err(|error| error.at(segment))
    }

    fn tuple_variant<V: de::Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let segment = Segment::Field(self.variant.clone());
        de::Deserializer::deserialize_seq(self.payload_deserializer()?, visitor)
            .map_err(|error| error.at(segment))
    }

    fn struct_variant<V: de::Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let segment = Segment::Field(self.variant.clone());
        de::Deserializer::deserialize_map(self.payload_deserializer()?, visitor)
            .map_err(|error| error.at(segment))
    }
}
