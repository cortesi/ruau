use super::{
    BridgeError, JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE, JSON_BRIDGE_LIGHTUSERDATA_TAG,
    JSON_NULL_LIGHTUSERDATA_HANDLE, Segment, json_number_to_f64,
};
use crate::{DEFAULT_MAX_VALUE_MARSHAL_DEPTH, MarshaledPair, MarshaledValue, scope::RuntimeError};

/// Converts an owned [`MarshaledValue`] into a [`serde_json::Value`] without
/// re-entering a scope.
///
/// # Errors
/// Returns [`RuntimeError`] when the value tree contains a value JSON cannot
/// represent or exceeds the marshal depth cap.
pub fn marshaled_to_json(value: &MarshaledValue) -> Result<serde_json::Value, RuntimeError> {
    marshaled_to_json_at(value, 0).map_err(BridgeError::into_runtime_error)
}

pub(super) fn marshaled_json_null() -> MarshaledValue {
    MarshaledValue::LightUserdata {
        handle: JSON_NULL_LIGHTUSERDATA_HANDLE,
        tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
    }
}

pub(super) fn marshaled_json_array_marker_pair() -> MarshaledPair {
    MarshaledPair {
        key: MarshaledValue::LightUserdata {
            handle: JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE,
            tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
        },
        value: MarshaledValue::Boolean(true),
    }
}

fn is_marshaled_json_null(value: &MarshaledValue) -> bool {
    matches!(
        value,
        MarshaledValue::LightUserdata {
            handle: JSON_NULL_LIGHTUSERDATA_HANDLE,
            tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
        }
    )
}

fn is_marshaled_json_array_marker_pair(pair: &MarshaledPair) -> bool {
    matches!(
        (&pair.key, &pair.value),
        (
            MarshaledValue::LightUserdata {
                handle: JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE,
                tag: JSON_BRIDGE_LIGHTUSERDATA_TAG,
            },
            MarshaledValue::Boolean(true),
        )
    )
}

fn marshaled_to_json_at(
    value: &MarshaledValue,
    depth: usize,
) -> Result<serde_json::Value, BridgeError> {
    if is_marshaled_json_null(value) {
        return Ok(serde_json::Value::Null);
    }
    match value {
        MarshaledValue::Nil => Ok(serde_json::Value::Null),
        MarshaledValue::Boolean(value) => Ok(serde_json::Value::Bool(*value)),
        MarshaledValue::Integer(value) => Ok(serde_json::Value::from(*value)),
        MarshaledValue::Number(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| {
                BridgeError::new(format!(
                    "non-finite number {value} is not representable in JSON"
                ))
            }),
        MarshaledValue::String(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Ok(serde_json::Value::String(text.to_owned())),
            Err(_) => Err(BridgeError::new(
                "non-UTF-8 string is not representable in JSON",
            )),
        },
        MarshaledValue::Table(pairs) => marshaled_table_to_json(pairs, depth),
        MarshaledValue::Vector(_) => Err(BridgeError::new("a vector is not representable in JSON")),
        MarshaledValue::Buffer(_) => Err(BridgeError::new("a buffer is not representable in JSON")),
        MarshaledValue::LightUserdata { .. } => Err(BridgeError::new(
            "light userdata is not representable in JSON",
        )),
        MarshaledValue::Opaque(kind) => Err(BridgeError::new(format!(
            "an opaque {kind} value is not representable in JSON"
        ))),
    }
}

/// The 1-based array index a marshaled key denotes within `1..=len`, if any.
fn marshaled_array_index(key: &MarshaledValue, len: usize) -> Option<usize> {
    let index = match key {
        MarshaledValue::Integer(value) => *value,
        #[expect(
            clippy::cast_possible_truncation,
            reason = "fract()==0 and the 1..=len bound keep the cast exact"
        )]
        MarshaledValue::Number(value) if value.fract() == 0.0 && *value >= 1.0 => *value as i64,
        _ => return None,
    };
    (index >= 1 && index as u128 <= len as u128).then_some(index as usize)
}

fn marshaled_table_to_json(
    pairs: &[MarshaledPair],
    depth: usize,
) -> Result<serde_json::Value, BridgeError> {
    if depth >= DEFAULT_MAX_VALUE_MARSHAL_DEPTH {
        return Err(BridgeError::depth());
    }
    let mut marker_seen = false;
    let mut marked_pairs = Vec::new();
    for pair in pairs {
        if is_marshaled_json_array_marker_pair(pair) {
            if marker_seen {
                return Err(BridgeError::new("duplicate JSON array marker"));
            }
            marker_seen = true;
        } else {
            marked_pairs.push(pair);
        }
    }
    if marker_seen {
        return marshaled_marked_array_to_json(&marked_pairs, depth);
    }
    if pairs.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    // Array attempt: integer keys covering exactly 1..n.
    let mut slots: Vec<Option<&MarshaledValue>> = vec![None; pairs.len()];
    let is_array = pairs.iter().all(|pair| {
        marshaled_array_index(&pair.key, pairs.len())
            .is_some_and(|index| slots[index - 1].replace(&pair.value).is_none())
    });
    if is_array {
        let mut items = Vec::with_capacity(pairs.len());
        for (index, slot) in slots.into_iter().enumerate() {
            let Some(value) = slot else {
                return Err(BridgeError::new("array table has a hole in keys 1..n")
                    .at(Segment::Index(index as u64 + 1)));
            };
            let value = marshaled_to_json_at(value, depth + 1)
                .map_err(|error| error.at(Segment::Index(index as u64 + 1)))?;
            items.push(value);
        }
        return Ok(serde_json::Value::Array(items));
    }
    // Object: every key must be a UTF-8 string.
    let mut map = serde_json::Map::new();
    for pair in pairs {
        let MarshaledValue::String(bytes) = &pair.key else {
            return Err(BridgeError::new(format!(
                "table key of type {} is not representable as a JSON object key",
                pair.key.type_name()
            )));
        };
        let Ok(key) = std::str::from_utf8(bytes) else {
            return Err(BridgeError::new(
                "non-UTF-8 table key is not representable as a JSON object key",
            ));
        };
        let value = marshaled_to_json_at(&pair.value, depth + 1)
            .map_err(|error| error.at(Segment::Field(key.to_owned())))?;
        map.insert(key.to_owned(), value);
    }
    Ok(serde_json::Value::Object(map))
}

fn marshaled_marked_array_to_json(
    pairs: &[&MarshaledPair],
    depth: usize,
) -> Result<serde_json::Value, BridgeError> {
    let mut slots: Vec<Option<&MarshaledValue>> = vec![None; pairs.len()];
    for pair in pairs {
        let Some(index) = marshaled_array_index(&pair.key, pairs.len()) else {
            return Err(BridgeError::new(
                "JSON array marker requires integer keys 1..n",
            ));
        };
        if slots[index - 1].replace(&pair.value).is_some() {
            return Err(BridgeError::new(
                "JSON array marker requires unique integer keys 1..n",
            ));
        }
    }
    let mut items = Vec::with_capacity(pairs.len());
    for (index, slot) in slots.into_iter().enumerate() {
        let Some(value) = slot else {
            return Err(
                BridgeError::new("JSON array marker table has a hole in keys 1..n")
                    .at(Segment::Index(index as u64 + 1)),
            );
        };
        let value = marshaled_to_json_at(value, depth + 1)
            .map_err(|error| error.at(Segment::Index(index as u64 + 1)))?;
        items.push(value);
    }
    Ok(serde_json::Value::Array(items))
}

/// Converts a [`serde_json::Value`] into an owned [`MarshaledValue`] tree.
///
/// JSON `null` becomes the Ruau-owned lightuserdata sentinel recognized by
/// [`marshaled_to_json`]. Arrays carry an Ruau-owned marker pair so empty
/// arrays stay distinct from empty objects on the owned path. Objects become
/// string-keyed tables. JSON integers map to `Integer`, floats to `Number`.
///
/// # Errors
/// Returns [`RuntimeError`] for integers above `i64::MAX` (no silent
/// precision loss) and for trees past the marshal depth cap. Messages are
/// prefixed with the path to the failing value.
/// Converts a [`serde_json::Value`] into an owned [`MarshaledValue`].
///
/// # Errors
/// Returns [`RuntimeError`] when a JSON integer is outside Lua's integer range
/// or the value tree exceeds the marshal depth cap.
pub fn json_to_marshaled(value: &serde_json::Value) -> Result<MarshaledValue, RuntimeError> {
    json_to_marshaled_at(value, 0).map_err(BridgeError::into_runtime_error)
}

fn json_to_marshaled_at(
    value: &serde_json::Value,
    depth: usize,
) -> Result<MarshaledValue, BridgeError> {
    match value {
        serde_json::Value::Null => Ok(marshaled_json_null()),
        serde_json::Value::Bool(value) => Ok(MarshaledValue::Boolean(*value)),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(MarshaledValue::Integer(value))
            } else if number.as_u64().is_some() {
                Err(BridgeError::new("integer out of range for Lua: u64"))
            } else {
                Ok(MarshaledValue::Number(json_number_to_f64(number)?))
            }
        }
        serde_json::Value::String(text) => Ok(MarshaledValue::String(text.clone().into_bytes())),
        serde_json::Value::Array(items) => {
            if depth >= DEFAULT_MAX_VALUE_MARSHAL_DEPTH {
                return Err(BridgeError::depth());
            }
            let mut pairs = Vec::with_capacity(items.len() + 1);
            pairs.push(marshaled_json_array_marker_pair());
            for (index, item) in items.iter().enumerate() {
                let value = json_to_marshaled_at(item, depth + 1)
                    .map_err(|error| error.at(Segment::Index(index as u64 + 1)))?;
                pairs.push(MarshaledPair {
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "array indices below 2^53 are exact in f64"
                    )]
                    key: MarshaledValue::Number((index + 1) as f64),
                    value,
                });
            }
            Ok(MarshaledValue::Table(pairs))
        }
        serde_json::Value::Object(map) => {
            if depth >= DEFAULT_MAX_VALUE_MARSHAL_DEPTH {
                return Err(BridgeError::depth());
            }
            let mut pairs = Vec::with_capacity(map.len());
            for (key, item) in map {
                let value = json_to_marshaled_at(item, depth + 1)
                    .map_err(|error| error.at(Segment::Field(key.clone())))?;
                pairs.push(MarshaledPair {
                    key: MarshaledValue::String(key.clone().into_bytes()),
                    value,
                });
            }
            Ok(MarshaledValue::Table(pairs))
        }
    }
}
