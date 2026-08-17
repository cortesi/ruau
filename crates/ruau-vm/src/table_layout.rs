//! Structural classification for Luau tables at host boundaries.
//!
//! This module deliberately stops at key layout. It does not coerce values,
//! stringify numeric keys, decode UTF-8, invoke metamethods, or prescribe a
//! domain value type. Embedders can apply their own policy after classification.

use crate::{MarshaledPair, ValueSnapshot, api::RawValue, vmutils};

/// The structural layout of a table's keys.
///
/// Dense sequences use exactly the positive integer keys `1..=len`. A table
/// containing both positive integer and string keys is [`Mixed`](Self::Mixed),
/// even when its integer portion is dense. Other key kinds are rejected rather
/// than being silently stringified.
#[derive(Clone, Debug, PartialEq)]
pub enum TableLayout {
    /// No entries.
    Empty,
    /// Contiguous positive integer keys beginning at one.
    Sequence {
        /// Number of sequence entries.
        len: usize,
    },
    /// String keys only.
    StringMap {
        /// Number of string-keyed entries.
        len: usize,
    },
    /// Positive integer keys with at least one gap.
    Sparse {
        /// Number of numeric entries.
        entries: usize,
        /// Largest positive integer key.
        max_index: u64,
        /// First absent positive integer index.
        first_missing: u64,
    },
    /// A combination of positive integer and string keys.
    Mixed {
        /// Number of positive integer entries.
        integer_keys: usize,
        /// Number of string entries.
        string_keys: usize,
    },
    /// A key that cannot participate in a sequence or string map.
    UnsupportedKey {
        /// Structured description of the first offending key in iteration
        /// order.
        key: UnsupportedTableKey,
    },
}

/// Marker-aware layout of a table produced by the JSON bridge.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonTableLayout {
    /// Layout of the ordinary table keys after a valid detached marker is removed.
    pub layout: TableLayout,
    /// Whether the table carries the protected JSON-array marker.
    pub marked_array: bool,
}

/// Why a table key is unsupported by [`TableLayout`].
#[derive(Clone, Debug, PartialEq)]
pub enum UnsupportedTableKey {
    /// An integer key at or below zero.
    NonPositiveInteger {
        /// The offending integer.
        value: i64,
    },
    /// A numeric key that is not an exact integer.
    FractionalNumber {
        /// Conservative Luau spelling for the number.
        display: String,
    },
    /// An exact positive numeric value outside the supported `u64` index range.
    IndexOutOfRange {
        /// Conservative Luau spelling for the number.
        display: String,
    },
    /// Two distinct Luau keys denote the same host sequence index (for example
    /// native integer `1` and number `1.0` in this VM revision).
    DuplicateIndex {
        /// The duplicated logical index.
        index: u64,
    },
    /// A key of a non-numeric, non-string Luau type.
    Type {
        /// Luau's ordinary type name for the key.
        type_name: &'static str,
    },
}

#[derive(Clone, Debug)]
enum LayoutKey {
    Integer(u64),
    String,
    Unsupported(UnsupportedTableKey),
}

/// Classifies an owned table snapshot without inspecting or cloning values.
#[must_use]
pub fn classify_marshaled_table(pairs: &[MarshaledPair]) -> TableLayout {
    classify_keys(pairs.iter().map(|pair| marshaled_key(&pair.key)))
}

/// Classifies an owned JSON table without treating its protected marker as a key.
///
/// Only a valid synthetic marker pair is removed. Ordinary fields, including a
/// string field named `__ruau_json_array`, remain part of the layout.
#[must_use]
pub fn classify_marshaled_json_table(pairs: &[MarshaledPair]) -> JsonTableLayout {
    let mut marked_array = false;
    let layout = classify_keys(pairs.iter().filter_map(|pair| {
        if crate::serde::is_marshaled_json_array_marker(pair) {
            marked_array = true;
            None
        } else {
            Some(marshaled_key(&pair.key))
        }
    }));
    JsonTableLayout {
        layout,
        marked_array,
    }
}

pub fn classify_raw_table_keys(keys: impl Iterator<Item = RawValue>) -> TableLayout {
    classify_keys(keys.map(raw_key))
}

pub fn classify_scoped_table_keys<'s>(
    keys: impl Iterator<Item = crate::ScopedValue<'s>>,
) -> TableLayout {
    classify_keys(keys.map(scoped_key))
}

fn classify_keys(keys: impl Iterator<Item = LayoutKey>) -> TableLayout {
    let mut integers = Vec::new();
    let mut string_keys = 0usize;

    for key in keys {
        match key {
            LayoutKey::Integer(index) => integers.push(index),
            LayoutKey::String => string_keys += 1,
            LayoutKey::Unsupported(key) => return TableLayout::UnsupportedKey { key },
        }
    }

    if integers.is_empty() {
        return if string_keys == 0 {
            TableLayout::Empty
        } else {
            TableLayout::StringMap { len: string_keys }
        };
    }
    if string_keys != 0 {
        return TableLayout::Mixed {
            integer_keys: integers.len(),
            string_keys,
        };
    }

    integers.sort_unstable();
    for window in integers.windows(2) {
        if window[0] == window[1] {
            return TableLayout::UnsupportedKey {
                key: UnsupportedTableKey::DuplicateIndex { index: window[0] },
            };
        }
    }
    let first_missing = integers
        .iter()
        .copied()
        .zip(1_u64..)
        .find_map(|(actual, expected)| (actual != expected).then_some(expected));
    match first_missing {
        None => TableLayout::Sequence {
            len: integers.len(),
        },
        Some(first_missing) => TableLayout::Sparse {
            entries: integers.len(),
            max_index: *integers.last().expect("integer list is non-empty"),
            first_missing,
        },
    }
}

fn integer_key(value: i64) -> LayoutKey {
    match u64::try_from(value) {
        Ok(value @ 1..) => LayoutKey::Integer(value),
        _ => LayoutKey::Unsupported(UnsupportedTableKey::NonPositiveInteger { value }),
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "range and integral guards make the conversion exact"
)]
fn number_key(value: f64) -> LayoutKey {
    if value.fract() != 0.0 {
        return LayoutKey::Unsupported(UnsupportedTableKey::FractionalNumber {
            display: vmutils::number_to_string(value),
        });
    }
    if value <= 0.0 {
        let value = if value >= i64::MIN as f64 {
            value as i64
        } else {
            i64::MIN
        };
        return LayoutKey::Unsupported(UnsupportedTableKey::NonPositiveInteger { value });
    }
    if value > u64::MAX as f64 {
        return LayoutKey::Unsupported(UnsupportedTableKey::IndexOutOfRange {
            display: vmutils::number_to_string(value),
        });
    }
    LayoutKey::Integer(value as u64)
}

fn raw_key(value: RawValue) -> LayoutKey {
    match value {
        RawValue::Integer(value) => integer_key(value),
        RawValue::Number(value) => number_key(value),
        RawValue::String(_) => LayoutKey::String,
        RawValue::Nil => unsupported_type("nil"),
        RawValue::Boolean(_) => unsupported_type("boolean"),
        RawValue::Vector(_) => unsupported_type("vector"),
        RawValue::LightUserdata { .. } | RawValue::Userdata(_) => unsupported_type("userdata"),
        RawValue::Table(_) => unsupported_type("table"),
        RawValue::Function(_) => unsupported_type("function"),
        RawValue::Thread(_) => unsupported_type("thread"),
        RawValue::Buffer(_) => unsupported_type("buffer"),
    }
}

fn scoped_key(value: crate::ScopedValue<'_>) -> LayoutKey {
    match value {
        crate::ScopedValue::Integer(value) => integer_key(value),
        crate::ScopedValue::Number(value) => number_key(value),
        crate::ScopedValue::String(_) => LayoutKey::String,
        other => unsupported_type(other.type_name()),
    }
}

fn marshaled_key(value: &ValueSnapshot) -> LayoutKey {
    match value {
        ValueSnapshot::Integer(value) => integer_key(*value),
        ValueSnapshot::Number(value) => number_key(*value),
        ValueSnapshot::String(_) => LayoutKey::String,
        other => unsupported_type(other.type_name()),
    }
}

fn unsupported_type(type_name: &'static str) -> LayoutKey {
    LayoutKey::Unsupported(UnsupportedTableKey::Type { type_name })
}

#[cfg(any())]
mod tests {
    use super::*;

    fn pairs(keys: impl IntoIterator<Item = ValueSnapshot>) -> Vec<MarshaledPair> {
        keys.into_iter()
            .map(|key| MarshaledPair {
                key,
                value: ValueSnapshot::Nil,
            })
            .collect()
    }

    #[test]
    fn classifies_marshaled_layouts() {
        assert_eq!(classify_marshaled_table(&[]), TableLayout::Empty);
        assert_eq!(
            classify_marshaled_table(&pairs([
                ValueSnapshot::Integer(2),
                ValueSnapshot::Number(1.0)
            ])),
            TableLayout::Sequence { len: 2 }
        );
        assert_eq!(
            classify_marshaled_table(&pairs([ValueSnapshot::String(b"a".to_vec())])),
            TableLayout::StringMap { len: 1 }
        );
        assert_eq!(
            classify_marshaled_table(&pairs([
                ValueSnapshot::Integer(1),
                ValueSnapshot::Integer(3)
            ])),
            TableLayout::Sparse {
                entries: 2,
                max_index: 3,
                first_missing: 2
            }
        );
        assert_eq!(
            classify_marshaled_table(&pairs([
                ValueSnapshot::Integer(1),
                ValueSnapshot::String(b"a".to_vec())
            ])),
            TableLayout::Mixed {
                integer_keys: 1,
                string_keys: 1
            }
        );
    }

    #[test]
    fn reports_marshaled_unsupported_keys() {
        let cases = [
            (
                ValueSnapshot::Integer(0),
                UnsupportedTableKey::NonPositiveInteger { value: 0 },
            ),
            (
                ValueSnapshot::Integer(-2),
                UnsupportedTableKey::NonPositiveInteger { value: -2 },
            ),
            (
                ValueSnapshot::Number(1.5),
                UnsupportedTableKey::FractionalNumber {
                    display: "1.5".to_owned(),
                },
            ),
            (
                ValueSnapshot::Boolean(true),
                UnsupportedTableKey::Type {
                    type_name: "boolean",
                },
            ),
        ];
        for (input, key) in cases {
            assert_eq!(
                classify_marshaled_table(&pairs([input])),
                TableLayout::UnsupportedKey { key }
            );
        }
    }

    #[test]
    fn rejects_duplicate_logical_indices() {
        assert_eq!(
            classify_marshaled_table(&pairs([
                ValueSnapshot::Integer(1),
                ValueSnapshot::Number(1.0)
            ])),
            TableLayout::UnsupportedKey {
                key: UnsupportedTableKey::DuplicateIndex { index: 1 }
            }
        );
    }

    #[test]
    fn handles_large_dense_tables() {
        let table = pairs((1..=100_000).map(ValueSnapshot::Integer));
        assert_eq!(
            classify_marshaled_table(&table),
            TableLayout::Sequence { len: 100_000 }
        );
    }
}
