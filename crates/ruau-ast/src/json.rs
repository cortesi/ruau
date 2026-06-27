//! Upstream AST JSON compatibility model.
//!
//! Upstream Luau's `AstJsonEncoder` emits a parser-facing JSON format that is
//! almost JSON, except for bare non-finite numeric tokens. This module parses
//! that format into typed envelope structures while preserving unknown fields
//! for the staged port.

use std::{collections::BTreeMap, fmt};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, Visitor},
    ser::{Error as SerError, SerializeMap, SerializeSeq},
};

use crate::{Location, Position};

/// A top-level `luau-ast` JSON document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JsonDocument {
    /// Parsed root AST node.
    pub root: JsonNode,
    /// Captured comment locations.
    #[serde(rename = "commentLocations")]
    pub comment_locations: Vec<JsonNode>,
}

/// A typed AST JSON object with a `type` tag.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JsonNode {
    /// Upstream JSON tag.
    #[serde(rename = "type")]
    pub kind: JsonKind,
    /// Optional source location.
    #[serde(default, deserialize_with = "deserialize_optional_location")]
    pub location: Option<Location>,
    /// Remaining upstream fields, preserved recursively.
    #[serde(flatten)]
    pub fields: BTreeMap<String, JsonValue>,
}

/// Known and unknown upstream AST JSON tags.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum JsonKind {
    /// A known upstream tag.
    Known(KnownJsonKind),
    /// A tag not yet modeled by this crate.
    Unknown(String),
}

impl Serialize for JsonKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Known(kind) => serializer.serialize_str(kind.as_ref()),
            Self::Unknown(tag) => {
                if tag.parse::<KnownJsonKind>().is_ok() {
                    Err(S::Error::custom(format!(
                        "unknown AST JSON tag `{tag}` shadows a known tag"
                    )))
                } else {
                    serializer.serialize_str(tag)
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for JsonKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let tag = String::deserialize(deserializer)?;
        Ok(tag
            .parse::<KnownJsonKind>()
            .map_or(Self::Unknown(tag), Self::Known))
    }
}

/// Known upstream AST JSON tags.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    strum::AsRefStr,
    strum::EnumString,
)]
pub enum KnownJsonKind {
    /// `AstArgumentName`.
    AstArgumentName,
    /// `AstAttr`.
    AstAttr,
    /// `AstDeclaredClassProp`.
    AstDeclaredClassProp,
    /// `AstExprBinary`.
    AstExprBinary,
    /// `AstExprCall`.
    AstExprCall,
    /// `AstExprConstantBool`.
    AstExprConstantBool,
    /// `AstExprConstantInteger`.
    AstExprConstantInteger,
    /// `AstExprConstantNil`.
    AstExprConstantNil,
    /// `AstExprConstantNumber`.
    AstExprConstantNumber,
    /// `AstExprConstantString`.
    AstExprConstantString,
    /// `AstExprError`.
    AstExprError,
    /// `AstExprFunction`.
    AstExprFunction,
    /// `AstExprGlobal`.
    AstExprGlobal,
    /// `AstExprGroup`.
    AstExprGroup,
    /// `AstExprIfElse`.
    AstExprIfElse,
    /// `AstExprIndexExpr`.
    AstExprIndexExpr,
    /// `AstExprIndexName`.
    AstExprIndexName,
    /// `AstExprInterpString`.
    AstExprInterpString,
    /// `AstExprLocal`.
    AstExprLocal,
    /// `AstExprTable`.
    AstExprTable,
    /// `AstExprTableItem`.
    AstExprTableItem,
    /// `AstExprTypeAssertion`.
    AstExprTypeAssertion,
    /// `AstExprUnary`.
    AstExprUnary,
    /// `AstExprVarargs`.
    AstExprVarargs,
    /// `AstGenericType`.
    AstGenericType,
    /// `AstGenericTypePack`.
    AstGenericTypePack,
    /// `AstLocal`.
    AstLocal,
    /// `AstStatAssign`.
    AstStatAssign,
    /// `AstStatBlock`.
    AstStatBlock,
    /// `AstStatBreak`.
    AstStatBreak,
    /// `AstStatCompoundAssign`.
    AstStatCompoundAssign,
    /// `AstStatContinue`.
    AstStatContinue,
    /// `AstStatDeclareClass`.
    AstStatDeclareClass,
    /// `AstStatDeclareFunction`.
    AstStatDeclareFunction,
    /// `AstStatDeclareGlobal`.
    AstStatDeclareGlobal,
    /// `AstStatError`.
    AstStatError,
    /// `AstStatExpr`.
    AstStatExpr,
    /// `AstStatFor`.
    AstStatFor,
    /// `AstStatForIn`.
    AstStatForIn,
    /// `AstStatFunction`.
    AstStatFunction,
    /// `AstStatIf`.
    AstStatIf,
    /// `AstStatLocal`.
    AstStatLocal,
    /// `AstStatLocalFunction`.
    AstStatLocalFunction,
    /// `AstStatRepeat`.
    AstStatRepeat,
    /// `AstStatReturn`.
    AstStatReturn,
    /// `AstStatTypeAlias`.
    AstStatTypeAlias,
    /// `AstStatWhile`.
    AstStatWhile,
    /// `AstTableProp`.
    AstTableProp,
    /// `AstTypeError`.
    AstTypeError,
    /// `AstTypeFunction`.
    AstTypeFunction,
    /// `AstTypeGroup`.
    AstTypeGroup,
    /// `AstTypeIntersection`.
    AstTypeIntersection,
    /// `AstTypeList`.
    AstTypeList,
    /// `AstTypeOptional`.
    AstTypeOptional,
    /// `AstTypePackExplicit`.
    AstTypePackExplicit,
    /// `AstTypePackGeneric`.
    AstTypePackGeneric,
    /// `AstTypePackVariadic`.
    AstTypePackVariadic,
    /// `AstTypeReference`.
    AstTypeReference,
    /// `AstTypeSingletonBool`.
    AstTypeSingletonBool,
    /// `AstTypeSingletonString`.
    AstTypeSingletonString,
    /// `AstTypeTable`.
    AstTypeTable,
    /// `AstTypeTypeof`.
    AstTypeTypeof,
    /// `AstTypeUnion`.
    AstTypeUnion,
    /// `BlockComment`.
    BlockComment,
    /// `BrokenComment`.
    BrokenComment,
    /// `Comment`.
    Comment,
}

/// Recursive AST JSON value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JsonValue {
    /// `null`.
    Null,
    /// Boolean value.
    Bool(bool),
    /// Numeric value.
    Number(JsonNumber),
    /// String value.
    String(String),
    /// Array value.
    Array(Vec<Self>),
    /// Untagged object value.
    Object(BTreeMap<String, Self>),
    /// Tagged AST JSON object.
    Node(Box<JsonNode>),
}

impl Serialize for JsonValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(value)?;
                }
                seq.end()
            }
            Self::Object(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
            Self::Node(node) => node.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        serde_json::Value::deserialize(deserializer).and_then(value_from_json)
    }
}

/// Upstream numeric representation.
#[derive(Clone, Debug)]
pub enum JsonNumber {
    /// A finite JSON number.
    Finite(serde_json::Number),
    /// Bare `Infinity`.
    Infinity,
    /// Bare `-Infinity`.
    NegativeInfinity,
    /// Bare `NaN`.
    Nan,
}

impl JsonNumber {
    /// Creates a finite AST JSON number.
    #[must_use]
    pub fn finite(value: f64) -> Option<Self> {
        serde_json::Number::from_f64(value).map(|value| Self::from_json_number(&value))
    }

    /// Creates an AST JSON number from a strict JSON number.
    #[must_use]
    pub fn from_json_number(value: &serde_json::Number) -> Self {
        Self::Finite(canonicalize_json_number(value))
    }

    /// Returns the finite numeric value as an `f64`.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Finite(value) => json_number_as_f64(value),
            Self::Infinity | Self::NegativeInfinity | Self::Nan => None,
        }
    }

    /// The `f64` value including the non-finite specials. Luau represents
    /// `Infinity`/`-Infinity`/`NaN` (e.g. an overflowing literal like `1e400`) as
    /// ordinary `double` constants, so a code generator emitting a number constant
    /// wants the actual IEEE value, not the finiteness-gated [`as_f64`]. `None` only
    /// when a `Finite` value is itself not representable as `f64`.
    #[must_use]
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            Self::Finite(value) => json_number_as_f64(value),
            Self::Infinity => Some(f64::INFINITY),
            Self::NegativeInfinity => Some(f64::NEG_INFINITY),
            Self::Nan => Some(f64::NAN),
        }
    }
}

impl PartialEq for JsonNumber {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Finite(left), Self::Finite(right)) => {
                json_number_as_f64(left) == json_number_as_f64(right)
            }
            (Self::Infinity, Self::Infinity)
            | (Self::NegativeInfinity, Self::NegativeInfinity)
            | (Self::Nan, Self::Nan) => true,
            _ => false,
        }
    }
}

impl Eq for JsonNumber {}

impl Serialize for JsonNumber {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Finite(value) => value.serialize(serializer),
            Self::Infinity => serializer.serialize_str(NON_FINITE_POSITIVE),
            Self::NegativeInfinity => serializer.serialize_str(NON_FINITE_NEGATIVE),
            Self::Nan => serializer.serialize_str(NON_FINITE_NAN),
        }
    }
}

/// Re-parses a number through the same serializer used by AST JSON output.
fn canonicalize_json_number(value: &serde_json::Number) -> serde_json::Number {
    let encoded =
        serde_json::to_string(&value).expect("serde_json::Number serializes for canonicalization");
    match serde_json::from_str(&encoded).expect("serialized JSON number parses") {
        serde_json::Value::Number(value) => value,
        _ => unreachable!("serialized JSON number remains a number"),
    }
}

/// Converts a JSON number through Rust's float parser.
fn json_number_as_f64(value: &serde_json::Number) -> Option<f64> {
    serde_json::to_string(value).ok()?.parse().ok()
}

/// Unary operation spelling from upstream JSON.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, strum::EnumString)]
pub enum JsonUnaryOp {
    /// Logical not.
    Not,
    /// Length.
    Len,
    /// Numeric negation.
    Minus,
}

/// Binary operation spelling from upstream JSON.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, strum::EnumString)]
pub enum JsonBinaryOp {
    /// Addition.
    Add,
    /// Subtraction.
    Sub,
    /// Multiplication.
    Mul,
    /// Division.
    Div,
    /// Floor division.
    FloorDiv,
    /// Modulo.
    Mod,
    /// Exponentiation.
    Pow,
    /// Concatenation.
    Concat,
    /// Logical and.
    And,
    /// Logical or.
    Or,
    /// Equality comparison.
    CompareEq,
    /// Inequality comparison.
    CompareNe,
    /// Less-than comparison.
    CompareLt,
    /// Less-or-equal comparison.
    CompareLe,
    /// Greater-than comparison.
    CompareGt,
    /// Greater-or-equal comparison.
    CompareGe,
}

/// Table item kind spelling from upstream JSON.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, strum::EnumString)]
pub enum JsonTableItemKind {
    /// Array-style item.
    #[strum(serialize = "item")]
    Item,
    /// Record-style field.
    #[strum(serialize = "record")]
    Record,
    /// General key/value pair.
    #[strum(serialize = "general")]
    General,
}

/// Compound assignment operation spelling from upstream JSON.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, strum::EnumString)]
pub enum JsonCompoundAssignOp {
    /// Addition assignment.
    Add,
    /// Subtraction assignment.
    Sub,
    /// Multiplication assignment.
    Mul,
    /// Division assignment.
    Div,
    /// Floor division assignment.
    FloorDiv,
    /// Modulo assignment.
    Mod,
    /// Power assignment.
    Pow,
    /// Concatenation assignment.
    Concat,
}

/// Parses a top-level upstream `luau-ast` JSON document.
pub fn parse_document(source: &str) -> Result<JsonDocument, serde_json::Error> {
    let mut document: JsonDocument = serde_json::from_str(&normalize_ast_json(source))?;
    restore_field_type_keys_node(&mut document.root);
    for comment in &mut document.comment_locations {
        restore_field_type_keys_node(comment);
    }
    Ok(document)
}

/// Parses a direct upstream AST JSON node.
pub fn parse_node(source: &str) -> Result<JsonNode, serde_json::Error> {
    let mut node: JsonNode = serde_json::from_str(&normalize_ast_json(source))?;
    restore_field_type_keys_node(&mut node);
    Ok(node)
}

/// Serializes a top-level AST JSON document.
pub fn to_document_string(document: &JsonDocument) -> Result<String, serde_json::Error> {
    serde_json::to_string(document).map(|json| denormalize_non_finite_numbers(&json))
}

/// Serializes a direct AST JSON node.
pub fn to_node_string(node: &JsonNode) -> Result<String, serde_json::Error> {
    serde_json::to_string(node).map(|json| denormalize_non_finite_numbers(&json))
}

/// Renumbers normalized adjacent-object fields in document order.
pub(crate) fn renumber_adjacent_fields(root: &mut JsonNode) {
    let mut next = 0usize;
    renumber_adjacent_fields_node(root, &mut next);
}

/// Converts serde JSON values into AST JSON values.
fn value_from_json<E>(value: serde_json::Value) -> Result<JsonValue, E>
where
    E: de::Error,
{
    match value {
        serde_json::Value::Null => Ok(JsonValue::Null),
        serde_json::Value::Bool(value) => Ok(JsonValue::Bool(value)),
        serde_json::Value::Number(value) => {
            Ok(JsonValue::Number(JsonNumber::from_json_number(&value)))
        }
        serde_json::Value::String(value) => Ok(match value.as_str() {
            NON_FINITE_POSITIVE => JsonValue::Number(JsonNumber::Infinity),
            NON_FINITE_NEGATIVE => JsonValue::Number(JsonNumber::NegativeInfinity),
            NON_FINITE_NAN => JsonValue::Number(JsonNumber::Nan),
            _ => JsonValue::String(value),
        }),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(value_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Array),
        serde_json::Value::Object(map) if map.contains_key("type") => {
            serde_json::from_value(serde_json::Value::Object(map))
                .map(|node| JsonValue::Node(Box::new(node)))
                .map_err(E::custom)
        }
        serde_json::Value::Object(map) => {
            let converted = map
                .into_iter()
                .map(|(key, value)| value_from_json(value).map(|value| (key, value)))
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            Ok(JsonValue::Object(converted))
        }
    }
}

/// Sentinel for upstream bare `Infinity`.
const NON_FINITE_POSITIVE: &str = "__ruau_non_finite_positive_infinity__";
/// Sentinel for upstream bare `-Infinity`.
const NON_FINITE_NEGATIVE: &str = "__ruau_non_finite_negative_infinity__";
/// Sentinel for upstream bare `NaN`.
const NON_FINITE_NAN: &str = "__ruau_non_finite_nan__";
/// Temporary key for upstream objects that have both a tag and a `type` field.
const FIELD_TYPE_KEY: &str = "__ruau_field_type";

/// Rewrites upstream AST JSON into strict JSON that serde can decode.
fn normalize_ast_json(source: &str) -> String {
    protect_duplicate_type_fields(&normalize_non_finite_numbers(source))
}

/// Rewrites upstream non-standard JSON into strict JSON.
fn normalize_non_finite_numbers(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut stack = Vec::new();
    let mut adjacent = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            let ch = source[index..]
                .chars()
                .next()
                .expect("index is within source");
            result.push(ch);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += ch.len_utf8();
            continue;
        }

        if byte == b'"' {
            in_string = true;
            result.push('"');
            index += 1;
        } else if matches!(byte, b'{' | b'[') {
            stack.push(byte);
            result.push(byte as char);
            if byte == b'[' && bytes.get(index + 1) == Some(&b',') {
                result.push_str("null");
            }
            index += 1;
        } else if byte == b'}' {
            if stack.last() == Some(&b'{') {
                stack.pop();
            }
            result.push('}');
            if bytes.get(index + 1) == Some(&b'{') {
                match stack.last().copied() {
                    Some(b'[') => result.push(','),
                    Some(b'{') => {
                        adjacent += 1;
                        result.push_str(&format!(",\"__ruau_adjacent_{adjacent}\":"));
                    }
                    _ => result.push(','),
                }
            }
            index += 1;
        } else if byte == b']' {
            if stack.last() == Some(&b'[') {
                stack.pop();
            }
            result.push(']');
            index += 1;
        } else if byte == b',' && bytes.get(index + 1) == Some(&b']') {
            index += 1;
        } else if byte == b',' && bytes.get(index + 1) == Some(&b',') {
            result.push_str(",null");
            index += 1;
        } else if byte == b':' && matches!(bytes.get(index + 1), Some(b',' | b'}' | b']')) {
            result.push_str(":null");
            index += 1;
        } else if source[index..].starts_with("-Infinity") {
            result.push('"');
            result.push_str(NON_FINITE_NEGATIVE);
            result.push('"');
            index += "-Infinity".len();
        } else if source[index..].starts_with("Infinity") {
            result.push('"');
            result.push_str(NON_FINITE_POSITIVE);
            result.push('"');
            index += "Infinity".len();
        } else if source[index..].starts_with("NaN") {
            result.push('"');
            result.push_str(NON_FINITE_NAN);
            result.push('"');
            index += "NaN".len();
        } else {
            let ch = source[index..]
                .chars()
                .next()
                .expect("index is within source");
            result.push(ch);
            index += ch.len_utf8();
        }
    }

    result
}

/// Protects duplicate `type` fields that would otherwise overwrite tag keys.
fn protect_duplicate_type_fields(source: &str) -> String {
    source.replace(",\"type\":{", &format!(",\"{FIELD_TYPE_KEY}\":{{"))
}

/// Rewrites strict JSON sentinel strings into upstream non-standard numbers.
fn denormalize_non_finite_numbers(source: &str) -> String {
    source
        .replace(&format!("\"{NON_FINITE_NEGATIVE}\""), "-Infinity")
        .replace(&format!("\"{NON_FINITE_POSITIVE}\""), "Infinity")
        .replace(&format!("\"{NON_FINITE_NAN}\""), "NaN")
}

/// Restores protected `type` fields in a parsed node.
fn restore_field_type_keys_node(node: &mut JsonNode) {
    if let Some(value) = node.fields.remove(FIELD_TYPE_KEY) {
        node.fields.insert("type".to_owned(), value);
    }

    for value in node.fields.values_mut() {
        restore_field_type_keys_value(value);
    }
}

/// Renumbers adjacent-object fields within a node.
fn renumber_adjacent_fields_node(node: &mut JsonNode, next: &mut usize) {
    for value in node.fields.values_mut() {
        renumber_adjacent_fields_value(value, next);
    }

    let adjacent_keys = node
        .fields
        .keys()
        .filter(|key| key.starts_with("__ruau_adjacent_"))
        .cloned()
        .collect::<Vec<_>>();

    for key in adjacent_keys {
        if let Some(value) = node.fields.remove(&key) {
            *next += 1;
            node.fields.insert(format!("__ruau_adjacent_{next}"), value);
        }
    }
}

/// Renumbers adjacent-object fields within a value.
fn renumber_adjacent_fields_value(value: &mut JsonValue, next: &mut usize) {
    match value {
        JsonValue::Array(values) => {
            for value in values {
                renumber_adjacent_fields_value(value, next);
            }
        }
        JsonValue::Object(values) => {
            for value in values.values_mut() {
                renumber_adjacent_fields_value(value, next);
            }
        }
        JsonValue::Node(node) => renumber_adjacent_fields_node(node, next),
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
    }
}

/// Restores protected `type` fields in a parsed value.
fn restore_field_type_keys_value(value: &mut JsonValue) {
    match value {
        JsonValue::Array(values) => {
            for value in values {
                restore_field_type_keys_value(value);
            }
        }
        JsonValue::Object(values) => {
            if let Some(value) = values.remove(FIELD_TYPE_KEY) {
                values.insert("type".to_owned(), value);
            }
            for value in values.values_mut() {
                restore_field_type_keys_value(value);
            }
        }
        JsonValue::Node(node) => restore_field_type_keys_node(node),
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
    }
}

/// Deserializes a required location string.
fn deserialize_location<'de, D>(deserializer: D) -> Result<Location, D::Error>
where
    D: Deserializer<'de>,
{
    let source = String::deserialize(deserializer)?;
    parse_location(&source).map_err(de::Error::custom)
}

/// Deserializes an optional location string.
fn deserialize_optional_location<'de, D>(deserializer: D) -> Result<Option<Location>, D::Error>
where
    D: Deserializer<'de>,
{
    struct OptionalLocationVisitor;

    impl<'de> Visitor<'de> for OptionalLocationVisitor {
        type Value = Option<Location>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("null or an upstream location string")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserialize_location(deserializer).map(Some)
        }
    }

    deserializer.deserialize_option(OptionalLocationVisitor)
}

/// Parses upstream location strings such as `1,2 - 3,4`.
fn parse_location(source: &str) -> Result<Location, String> {
    let (begin, end) = source
        .split_once(" - ")
        .ok_or_else(|| format!("invalid location `{source}`"))?;
    Ok(Location::new(parse_position(begin)?, parse_position(end)?))
}

/// Parses upstream position strings such as `1,2`.
fn parse_position(source: &str) -> Result<Position, String> {
    let (line, column) = source
        .split_once(',')
        .ok_or_else(|| format!("invalid position `{source}`"))?;
    Ok(Position::new(
        line.parse()
            .map_err(|error| format!("invalid line `{line}`: {error}"))?,
        column
            .parse()
            .map_err(|error| format!("invalid column `{column}`: {error}"))?,
    ))
}

#[cfg(any())]
mod tests {
    use super::{
        JsonKind, JsonNumber, JsonValue, KnownJsonKind, parse_document, parse_node,
        to_document_string, to_node_string,
    };
    use crate::{Location, Position};

    #[test]
    fn parses_top_level_document() {
        let document = parse_document(
            r#"{"root":{"type":"AstStatBlock","location":"0,0 - 0,0","hasEnd":true,"body":[]},"commentLocations":[]}"#,
        )
        .expect("document parses");

        assert_eq!(
            document.root.kind,
            JsonKind::Known(KnownJsonKind::AstStatBlock)
        );
        assert_eq!(
            document.root.location,
            Some(Location::new(Position::new(0, 0), Position::new(0, 0)))
        );
    }

    #[test]
    fn parses_direct_node_with_non_finite_number() {
        let node = parse_node(
            r#"{"type":"AstExprConstantNumber","location":"0,0 - 0,0","value":Infinity}"#,
        )
        .expect("node parses");

        assert_eq!(
            node.fields.get("value"),
            Some(&JsonValue::Number(JsonNumber::Infinity))
        );
    }

    #[test]
    fn serializes_direct_node_with_non_finite_number() {
        let node = parse_node(
            r#"{"type":"AstExprConstantNumber","location":"0,0 - 0,0","value":-Infinity}"#,
        )
        .expect("node parses");

        let encoded = to_node_string(&node).expect("node serializes");
        assert!(encoded.contains(r#""type":"AstExprConstantNumber""#));
        assert!(encoded.contains(r#""location":"0,0 - 0,0""#));
        assert!(encoded.contains(r#""value":-Infinity"#));
        assert_eq!(parse_node(&encoded).expect("encoded node parses"), node);
    }

    #[test]
    fn finite_number_equality_is_structural() {
        let integer = JsonNumber::from_json_number(&serde_json::Number::from(1));
        let float = JsonNumber::finite(1.0).expect("1.0 is finite");

        assert_eq!(integer, float);
    }

    #[test]
    fn exponent_number_equality_uses_float_value() {
        let from_upstream_text =
            JsonNumber::from_json_number(&serde_json::from_str("1.0000000000000001e-09").unwrap());
        let from_rust_float = JsonNumber::finite(1e-9).expect("1e-9 is finite");

        assert_eq!(from_upstream_text, from_rust_float);
    }

    #[test]
    fn preserves_string_escaping() {
        let node = parse_node(
            "{\"type\":\"AstExprConstantString\",\"location\":\"0,0 - 0,0\",\"value\":\"a\\u001d\\u0000\\\\\\\"b\"}",
        )
        .expect("node parses");

        assert!(matches!(
            node.fields.get("value"),
            Some(JsonValue::String(value)) if value.contains('\0')
        ));
    }

    #[test]
    fn tolerates_adjacent_objects_from_unencoded_upstream_nodes() {
        let document = parse_document(
            r#"{"root":{"type":"AstStatBlock","location":"0,0 - 0,0","hasEnd":true,"body":[{"type":"AstExprLocal","location":"0,0 - 0,1"}{"type":"AstTypeReference","location":"0,0 - 0,1","name":"T","nameLocation":"0,0 - 0,1","parameters":[]}]},"commentLocations":[]}"#,
        )
        .expect("document parses");

        assert!(document.root.fields.contains_key("body"));
    }

    #[test]
    fn round_trips_top_level_document_structurally() {
        let document = parse_document(
            r#"{"root":{"type":"AstStatBlock","location":"0,0 - 0,0","hasEnd":true,"body":[{"type":"AstStatReturn","location":"0,0 - 0,10","list":[{"type":"AstExprConstantNumber","location":"0,7 - 0,10","value":NaN}]}]},"commentLocations":[{"type":"Comment","location":"1,0 - 1,8"}]}"#,
        )
        .expect("document parses");

        let encoded = to_document_string(&document).expect("document serializes");
        let reparsed = parse_document(&encoded).expect("encoded document parses");

        assert_eq!(reparsed, document);
    }

    #[test]
    fn rejects_unknown_tag_that_shadows_known_tag() {
        let node = super::JsonNode {
            kind: JsonKind::Unknown("AstStatBlock".to_owned()),
            location: None,
            fields: Default::default(),
        };

        assert!(to_node_string(&node).is_err());
    }
}
