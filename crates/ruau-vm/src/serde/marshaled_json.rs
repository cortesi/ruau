use super::{
    BridgeError, JSON_ARRAY_MARKER_LIGHTUSERDATA_HANDLE, JSON_BRIDGE_LIGHTUSERDATA_TAG,
    JSON_NULL_LIGHTUSERDATA_HANDLE, Segment, json_number_to_f64,
};
use crate::{DEFAULT_MAX_VALUE_MARSHAL_DEPTH, MarshaledPair, MarshaledValue, scope::RuntimeError};

/// Number conversion policy for [`marshaled_to_json_with_options`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonNumberPolicy {
    /// Preserve the VM value kind: [`MarshaledValue::Number`] stays a JSON
    /// floating-point number even when it is integral.
    PreserveValueKind,
    /// Convert exactly-integral finite [`MarshaledValue::Number`] values that
    /// fit in `i64` into JSON integers.
    IntegralFloatsToIntegers,
}

/// Sparse-array conversion policy for [`marshaled_to_json_with_options`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonSparseArrayPolicy {
    /// Reject arrays whose integer keys do not cover exactly `1..n`.
    Reject,
    /// Fill missing array slots with JSON `null`.
    NullFill,
}

/// Options for converting owned marshaled VM values into JSON.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarshaledJsonOptions {
    number_policy: JsonNumberPolicy,
    sparse_array_policy: JsonSparseArrayPolicy,
}

impl Default for MarshaledJsonOptions {
    fn default() -> Self {
        Self {
            number_policy: JsonNumberPolicy::PreserveValueKind,
            sparse_array_policy: JsonSparseArrayPolicy::Reject,
        }
    }
}

impl MarshaledJsonOptions {
    /// Strict compatibility with [`marshaled_to_json`].
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            number_policy: JsonNumberPolicy::PreserveValueKind,
            sparse_array_policy: JsonSparseArrayPolicy::Reject,
        }
    }

    /// Converts exactly-integral finite VM numbers to JSON integers.
    #[must_use]
    pub const fn integral_floats_to_integers(mut self) -> Self {
        self.number_policy = JsonNumberPolicy::IntegralFloatsToIntegers;
        self
    }

    /// Fills sparse array holes with JSON `null` instead of rejecting them.
    #[must_use]
    pub const fn null_fill_sparse_arrays(mut self) -> Self {
        self.sparse_array_policy = JsonSparseArrayPolicy::NullFill;
        self
    }

    /// The configured number policy.
    #[must_use]
    pub const fn number_policy(self) -> JsonNumberPolicy {
        self.number_policy
    }

    /// The configured sparse-array policy.
    #[must_use]
    pub const fn sparse_array_policy(self) -> JsonSparseArrayPolicy {
        self.sparse_array_policy
    }
}

/// Converts an owned [`MarshaledValue`] into a [`serde_json::Value`] without
/// re-entering a scope.
///
/// `nil` maps to JSON `null`; scalar booleans, integers, finite numbers, and
/// UTF-8 strings map directly; table snapshots with integer keys `1..n` map to
/// arrays; table snapshots with string keys map to objects; an empty unmarked
/// table maps to `{}`. Vectors, buffers, non-reserved light userdata, opaque
/// values, non-UTF-8 strings, non-finite numbers, and mixed or gapped table
/// shapes are rejected.
///
/// # Errors
/// Returns [`RuntimeError`] when the value tree contains a value JSON cannot
/// represent or exceeds the marshal depth cap.
pub fn marshaled_to_json(value: &MarshaledValue) -> Result<serde_json::Value, RuntimeError> {
    marshaled_to_json_with_options(value, MarshaledJsonOptions::strict())
}

/// Converts an owned [`MarshaledValue`] into a [`serde_json::Value`] with
/// explicit JSON conversion policies.
///
/// # Errors
/// Returns [`RuntimeError`] when the value tree contains a value JSON cannot
/// represent or exceeds the marshal depth cap.
pub fn marshaled_to_json_with_options(
    value: &MarshaledValue,
    options: MarshaledJsonOptions,
) -> Result<serde_json::Value, RuntimeError> {
    marshaled_to_json_at(value, 0, options).map_err(BridgeError::into_runtime_error)
}

/// Converts multiple returned [`MarshaledValue`]s into a JSON array.
///
/// This is the direct bridge for ordinary multi-return VM results when the
/// caller wants to preserve the return list shape.
///
/// # Errors
/// Returns [`RuntimeError`] when any returned value cannot be represented as
/// JSON. Error paths are prefixed with the 1-based return slot (`[1]`, `[2]`,
/// ...).
pub fn marshaled_values_to_json_array(
    values: &[MarshaledValue],
) -> Result<serde_json::Value, RuntimeError> {
    let mut items = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let slot = u64::try_from(index + 1).expect("return slot index fits in u64");
        let item = marshaled_to_json_at(value, 0, MarshaledJsonOptions::strict())
            .map_err(|error| error.at(Segment::Index(slot)))
            .map_err(BridgeError::into_runtime_error)?;
        items.push(item);
    }
    Ok(serde_json::Value::Array(items))
}

/// Converts multiple returned [`MarshaledValue`]s into a JSON array with
/// explicit JSON conversion policies.
///
/// # Errors
/// Returns [`RuntimeError`] when any returned value cannot be represented as
/// JSON. Error paths are prefixed with the 1-based return slot.
pub fn marshaled_values_to_json_array_with_options(
    values: &[MarshaledValue],
    options: MarshaledJsonOptions,
) -> Result<serde_json::Value, RuntimeError> {
    let mut items = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let slot = u64::try_from(index + 1).expect("return slot index fits in u64");
        let item = marshaled_to_json_at(value, 0, options)
            .map_err(|error| error.at(Segment::Index(slot)))
            .map_err(BridgeError::into_runtime_error)?;
        items.push(item);
    }
    Ok(serde_json::Value::Array(items))
}

/// Converts returned [`MarshaledValue`]s into the common host JSON return
/// shape: no values become `None`, one value becomes that JSON value, and
/// multiple values become a JSON array.
///
/// # Errors
/// Returns [`RuntimeError`] when any returned value cannot be represented as
/// JSON.
pub fn marshaled_return_values_to_json(
    values: &[MarshaledValue],
) -> Result<Option<serde_json::Value>, RuntimeError> {
    marshaled_return_values_to_json_with_options(values, MarshaledJsonOptions::strict())
}

/// Converts returned [`MarshaledValue`]s into the common host JSON return shape
/// with explicit JSON conversion policies.
///
/// # Errors
/// Returns [`RuntimeError`] when any returned value cannot be represented as
/// JSON.
pub fn marshaled_return_values_to_json_with_options(
    values: &[MarshaledValue],
    options: MarshaledJsonOptions,
) -> Result<Option<serde_json::Value>, RuntimeError> {
    match values {
        [] => Ok(None),
        [value] => marshaled_to_json_with_options(value, options).map(Some),
        _ => marshaled_values_to_json_array_with_options(values, options).map(Some),
    }
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
    options: MarshaledJsonOptions,
) -> Result<serde_json::Value, BridgeError> {
    if is_marshaled_json_null(value) {
        return Ok(serde_json::Value::Null);
    }
    match value {
        MarshaledValue::Nil => Ok(serde_json::Value::Null),
        MarshaledValue::Boolean(value) => Ok(serde_json::Value::Bool(*value)),
        MarshaledValue::Integer(value) => Ok(serde_json::Value::from(*value)),
        MarshaledValue::Number(value) => marshaled_number_to_json(*value, options),
        MarshaledValue::String(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Ok(serde_json::Value::String(text.to_owned())),
            Err(_) => Err(BridgeError::new(
                "non-UTF-8 string is not representable in JSON",
            )),
        },
        MarshaledValue::Table(pairs) => marshaled_table_to_json(pairs, depth, options),
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

fn marshaled_number_to_json(
    value: f64,
    options: MarshaledJsonOptions,
) -> Result<serde_json::Value, BridgeError> {
    if !value.is_finite() {
        return Err(BridgeError::new(format!(
            "non-finite number {value} is not representable in JSON"
        )));
    }
    if options.number_policy == JsonNumberPolicy::IntegralFloatsToIntegers
        && let Some(integer) = exact_i64_from_f64(value)
    {
        return Ok(serde_json::Value::from(integer));
    }
    serde_json::Number::from_f64(value)
        .map(serde_json::Value::Number)
        .ok_or_else(|| {
            BridgeError::new(format!(
                "non-finite number {value} is not representable in JSON"
            ))
        })
}

fn exact_i64_from_f64(value: f64) -> Option<i64> {
    if !value.is_finite() || value.fract() != 0.0 || (value == 0.0 && value.is_sign_negative()) {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the round-trip check below rejects values outside the exact i64 range"
    )]
    let integer = value as i64;
    ((integer as f64) == value).then_some(integer)
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

/// The 1-based array index a marshaled key denotes, if it is a positive integer.
fn marshaled_positive_array_index(key: &MarshaledValue) -> Option<usize> {
    let index = match key {
        MarshaledValue::Integer(value) => *value,
        #[expect(
            clippy::cast_possible_truncation,
            reason = "fract()==0 and the round-trip check keep the cast exact"
        )]
        MarshaledValue::Number(value) if value.fract() == 0.0 && *value >= 1.0 => {
            let index = *value as i64;
            if (index as f64) != *value {
                return None;
            }
            index
        }
        _ => return None,
    };
    usize::try_from(index).ok().filter(|index| *index >= 1)
}

fn marshaled_table_to_json(
    pairs: &[MarshaledPair],
    depth: usize,
    options: MarshaledJsonOptions,
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
        return marshaled_marked_array_to_json(&marked_pairs, depth, options);
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
            let value = marshaled_to_json_at(value, depth + 1, options)
                .map_err(|error| error.at(Segment::Index(index as u64 + 1)))?;
            items.push(value);
        }
        return Ok(serde_json::Value::Array(items));
    }
    if options.sparse_array_policy == JsonSparseArrayPolicy::NullFill
        && let Some(array) = marshaled_sparse_array_to_json(pairs, depth, options)?
    {
        return Ok(array);
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
        let value = marshaled_to_json_at(&pair.value, depth + 1, options)
            .map_err(|error| error.at(Segment::Field(key.to_owned())))?;
        map.insert(key.to_owned(), value);
    }
    Ok(serde_json::Value::Object(map))
}

fn marshaled_marked_array_to_json(
    pairs: &[&MarshaledPair],
    depth: usize,
    options: MarshaledJsonOptions,
) -> Result<serde_json::Value, BridgeError> {
    let len = if options.sparse_array_policy == JsonSparseArrayPolicy::NullFill {
        pairs
            .iter()
            .filter_map(|pair| marshaled_positive_array_index(&pair.key))
            .max()
            .unwrap_or(0)
    } else {
        pairs.len()
    };
    let mut slots: Vec<Option<&MarshaledValue>> = vec![None; len];
    for pair in pairs {
        let Some(index) = (if options.sparse_array_policy == JsonSparseArrayPolicy::NullFill {
            marshaled_positive_array_index(&pair.key)
        } else {
            marshaled_array_index(&pair.key, pairs.len())
        }) else {
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
            if options.sparse_array_policy == JsonSparseArrayPolicy::NullFill {
                items.push(serde_json::Value::Null);
                continue;
            }
            return Err(
                BridgeError::new("JSON array marker table has a hole in keys 1..n")
                    .at(Segment::Index(index as u64 + 1)),
            );
        };
        let value = marshaled_to_json_at(value, depth + 1, options)
            .map_err(|error| error.at(Segment::Index(index as u64 + 1)))?;
        items.push(value);
    }
    Ok(serde_json::Value::Array(items))
}

fn marshaled_sparse_array_to_json(
    pairs: &[MarshaledPair],
    depth: usize,
    options: MarshaledJsonOptions,
) -> Result<Option<serde_json::Value>, BridgeError> {
    let Some(max_index) = pairs
        .iter()
        .map(|pair| marshaled_positive_array_index(&pair.key))
        .collect::<Option<Vec<_>>>()
        .and_then(|indices| indices.into_iter().max())
    else {
        return Ok(None);
    };
    let mut slots: Vec<Option<&MarshaledValue>> = vec![None; max_index];
    for pair in pairs {
        let index = marshaled_positive_array_index(&pair.key).expect("checked above");
        if slots[index - 1].replace(&pair.value).is_some() {
            return Err(BridgeError::new(
                "sparse array requires unique positive integer keys",
            ));
        }
    }
    let mut items = Vec::with_capacity(slots.len());
    for (index, slot) in slots.into_iter().enumerate() {
        let Some(value) = slot else {
            items.push(serde_json::Value::Null);
            continue;
        };
        let value = marshaled_to_json_at(value, depth + 1, options)
            .map_err(|error| error.at(Segment::Index(index as u64 + 1)))?;
        items.push(value);
    }
    Ok(Some(serde_json::Value::Array(items)))
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
