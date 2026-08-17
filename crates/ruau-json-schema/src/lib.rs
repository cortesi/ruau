//! Deterministic JSON Schema to typed Luau declaration lowering.
//!
//! The lowerer supports object and scalar types, arrays and tuples, local
//! fragment references, `allOf`, `oneOf`, `anyOf`, `const`, `enum`, and boolean
//! schemas. Named object properties stay sealed unless `additionalProperties`
//! is present. An object without properties lowers to `{ [string]: unknown }`.
//! External references and constraints without a sound Luau type are widened
//! with structured diagnostics.

use std::collections::{BTreeMap, BTreeSet};

use ruau_declaration::{Field, TableIndexer, Type};
use serde_json::{Map, Value};

/// Stable category for one lowering diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiagnosticCode {
    /// A schema form or constraint has no sound representation.
    UnsupportedSchema,
    /// A local reference is missing or an external reference was requested.
    UnresolvedReference,
    /// Local references form a cycle.
    ReferenceCycle,
    /// `allOf` members cannot be merged as one object type.
    IncompatibleAllOf,
    /// A literal was widened to its Luau category.
    LiteralWidened,
    /// A traversal limit was reached.
    LimitExceeded,
}

/// One generic JSON Schema lowering diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    /// Stable diagnostic category.
    pub code: DiagnosticCode,
    /// RFC 6901 pointer to the affected schema fragment.
    pub pointer: String,
    /// Human-readable failure detail.
    pub message: String,
}

/// Defensive traversal limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Maximum recursive schema depth.
    pub max_depth: usize,
    /// Maximum visited schema fragments.
    pub max_nodes: usize,
    /// Maximum active local-reference chain length.
    pub max_references: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_nodes: 10_000,
            max_references: 256,
        }
    }
}

/// One lowered type and every conservative decision made for it.
#[derive(Clone, Debug, PartialEq)]
pub struct LoweredSchema {
    /// Resulting Luau type.
    pub ty: Type,
    /// Diagnostics in deterministic traversal order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lowers one JSON Schema document with default limits.
#[must_use]
pub fn lower(schema: &Value) -> LoweredSchema {
    lower_with_limits(schema, Limits::default())
}

/// Lowers one JSON Schema document with explicit defensive limits.
#[must_use]
pub fn lower_with_limits(schema: &Value, limits: Limits) -> LoweredSchema {
    Lowerer {
        root: schema,
        references: Vec::new(),
        diagnostics: Vec::new(),
        limits,
        nodes: 0,
    }
    .finish()
}

struct Lowerer<'a> {
    root: &'a Value,
    references: Vec<String>,
    diagnostics: Vec<Diagnostic>,
    limits: Limits,
    nodes: usize,
}

impl<'a> Lowerer<'a> {
    fn finish(mut self) -> LoweredSchema {
        let ty = self.lower_at(self.root, "", 0);
        LoweredSchema {
            ty,
            diagnostics: self.diagnostics,
        }
    }

    fn lower_at(&mut self, schema: &'a Value, pointer: &str, depth: usize) -> Type {
        self.nodes = self.nodes.saturating_add(1);
        if depth > self.limits.max_depth || self.nodes > self.limits.max_nodes {
            self.diagnostic(
                DiagnosticCode::LimitExceeded,
                pointer,
                "JSON Schema traversal limit exceeded",
            );
            return Type::Unknown;
        }
        let Some(object) = schema.as_object() else {
            return match schema {
                Value::Bool(true) => Type::Unknown,
                Value::Bool(false) => {
                    self.diagnostic(
                        DiagnosticCode::UnsupportedSchema,
                        pointer,
                        "false schemas cannot be represented precisely",
                    );
                    Type::Unknown
                }
                _ => {
                    self.diagnostic(
                        DiagnosticCode::UnsupportedSchema,
                        pointer,
                        "schema fragment is not an object or boolean",
                    );
                    Type::Unknown
                }
            };
        };

        if object
            .keys()
            .any(|key| UNSUPPORTED_CONSTRAINTS.contains(&key.as_str()))
        {
            self.diagnostic(
                DiagnosticCode::UnsupportedSchema,
                pointer,
                "schema uses constraints that Luau declarations cannot represent",
            );
            return Type::Unknown;
        }

        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            return self.lower_reference(reference, pointer, depth + 1);
        }
        if let Some(value) = object.get("const") {
            return self.lower_const(value, pointer);
        }
        if let Some(values) = object.get("enum").and_then(Value::as_array) {
            return self.lower_enum(values, pointer);
        }
        if let Some(values) = object.get("allOf").and_then(Value::as_array) {
            return self.lower_all_of(values, pointer, depth + 1);
        }
        for keyword in ["oneOf", "anyOf"] {
            if let Some(values) = object.get(keyword).and_then(Value::as_array) {
                if values.is_empty() {
                    self.diagnostic(
                        DiagnosticCode::UnsupportedSchema,
                        pointer,
                        format!("empty `{keyword}` cannot be represented"),
                    );
                    return Type::Unknown;
                }
                let mut variants = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    variants.push(self.lower_at(
                        value,
                        &child(pointer, &format!("{keyword}/{index}")),
                        depth + 1,
                    ));
                }
                return schema_union(variants);
            }
        }
        if let Some(types) = object.get("type").and_then(Value::as_array) {
            if types.is_empty() {
                self.diagnostic(
                    DiagnosticCode::UnsupportedSchema,
                    pointer,
                    "empty schema type array cannot be represented",
                );
                return Type::Unknown;
            }
            let mut variants = Vec::with_capacity(types.len());
            for (index, value) in types.iter().enumerate() {
                let Some(name) = value.as_str() else {
                    self.diagnostic(
                        DiagnosticCode::UnsupportedSchema,
                        &child(pointer, &format!("type/{index}")),
                        "schema type array contains a non-string member",
                    );
                    variants.push(Type::Unknown);
                    continue;
                };
                variants.push(self.lower_type(name, object, pointer, depth + 1));
            }
            return schema_union(variants);
        }
        match object.get("type").and_then(Value::as_str) {
            Some(name) => self.lower_type(name, object, pointer, depth + 1),
            None if object.contains_key("properties")
                || object.contains_key("additionalProperties") =>
            {
                self.lower_object(object, pointer, depth + 1)
            }
            None if is_permissive_schema(object) => Type::Unknown,
            None => {
                self.diagnostic(
                    DiagnosticCode::UnsupportedSchema,
                    pointer,
                    "constrained schema has no supported type",
                );
                Type::Unknown
            }
        }
    }

    fn lower_type(
        &mut self,
        name: &str,
        object: &'a Map<String, Value>,
        pointer: &str,
        depth: usize,
    ) -> Type {
        match name {
            "null" => Type::Nil,
            "string" => Type::String,
            "number" | "integer" => Type::Number,
            "boolean" => Type::Boolean,
            "array" => self.lower_array(object, pointer, depth),
            "object" => self.lower_object(object, pointer, depth),
            _ => {
                self.diagnostic(
                    DiagnosticCode::UnsupportedSchema,
                    pointer,
                    format!("unsupported JSON Schema type `{name}`"),
                );
                Type::Unknown
            }
        }
    }

    fn lower_array(&mut self, object: &'a Map<String, Value>, pointer: &str, depth: usize) -> Type {
        if let Some(items) = object.get("prefixItems").and_then(Value::as_array) {
            let mut members = Vec::with_capacity(items.len() + 1);
            for (index, value) in items.iter().enumerate() {
                members.push(self.lower_at(
                    value,
                    &child(pointer, &format!("prefixItems/{index}")),
                    depth + 1,
                ));
            }
            match object.get("items") {
                Some(Value::Bool(false)) => {}
                Some(items) => {
                    members.push(self.lower_at(items, &child(pointer, "items"), depth + 1))
                }
                None => members.push(Type::Unknown),
            }
            return if members.iter().any(|member| member == &Type::Unknown) {
                Type::Unknown.array()
            } else {
                Type::tuple(members)
            };
        }
        object.get("items").map_or_else(
            || Type::Unknown.array(),
            |value| {
                self.lower_at(value, &child(pointer, "items"), depth + 1)
                    .array()
            },
        )
    }

    fn lower_object(
        &mut self,
        object: &'a Map<String, Value>,
        pointer: &str,
        depth: usize,
    ) -> Type {
        let properties = object.get("properties").and_then(Value::as_object);
        let required = object
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<BTreeSet<_>>();
        let named_properties = properties.is_some_and(|properties| !properties.is_empty());
        let additional = self.lower_additional(object, pointer, depth, named_properties);
        let Some(properties) = properties else {
            return additional
                .map_or_else(|| Type::table([]), |value| Type::map(Type::String, value));
        };
        let mut fields = Vec::with_capacity(properties.len());
        for (name, schema) in properties {
            let mut value = self.lower_at(
                schema,
                &child(pointer, &format!("properties/{}", escape(name))),
                depth + 1,
            );
            if !required.contains(name.as_str()) {
                value = value.optional();
            }
            let field = Field::new(name.clone(), value);
            fields.push(
                schema
                    .as_object()
                    .and_then(description)
                    .map_or(field.clone(), |documentation| field.doc(documentation)),
            );
        }
        match additional {
            Some(value) => Type::table_with_indexer(fields, TableIndexer::new(Type::String, value)),
            None => Type::table(fields),
        }
    }

    fn lower_additional(
        &mut self,
        object: &'a Map<String, Value>,
        pointer: &str,
        depth: usize,
        named_properties: bool,
    ) -> Option<Type> {
        match object.get("additionalProperties") {
            Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(Type::Unknown),
            // Named fields without additionalProperties stay sealed. An implicit
            // unknown indexer on those fields is not a usable write type.
            None if named_properties => None,
            None => Some(Type::Unknown),
            Some(schema) => {
                Some(self.lower_at(schema, &child(pointer, "additionalProperties"), depth + 1))
            }
        }
    }

    fn lower_reference(&mut self, reference: &str, pointer: &str, depth: usize) -> Type {
        let Some(fragment) = reference.strip_prefix('#') else {
            self.diagnostic(
                DiagnosticCode::UnresolvedReference,
                pointer,
                format!("external schema reference `{reference}` is unsupported"),
            );
            return Type::Unknown;
        };
        if self.references.iter().any(|active| active == reference) {
            self.diagnostic(
                DiagnosticCode::ReferenceCycle,
                pointer,
                format!("local schema reference cycle through `{reference}`"),
            );
            return Type::Unknown;
        }
        if self.references.len() >= self.limits.max_references {
            self.diagnostic(
                DiagnosticCode::LimitExceeded,
                pointer,
                "local schema reference limit exceeded",
            );
            return Type::Unknown;
        }
        let root = self.root;
        let Some(target) = root.pointer(fragment) else {
            self.diagnostic(
                DiagnosticCode::UnresolvedReference,
                pointer,
                format!("local schema reference `{reference}` does not exist"),
            );
            return Type::Unknown;
        };
        self.references.push(reference.to_owned());
        let lowered = self.lower_at(target, fragment, depth + 1);
        self.references.pop();
        lowered
    }

    fn lower_all_of(&mut self, values: &'a [Value], pointer: &str, depth: usize) -> Type {
        if values.is_empty() {
            self.diagnostic(
                DiagnosticCode::IncompatibleAllOf,
                pointer,
                "empty `allOf` cannot be composed",
            );
            return Type::Unknown;
        }
        let mut merged = None;
        for (index, value) in values.iter().enumerate() {
            let member_pointer = child(pointer, &format!("allOf/{index}"));
            let member = self.lower_at(value, &member_pointer, depth + 1);
            let Some(table) = table_parts(member) else {
                self.diagnostic(
                    DiagnosticCode::IncompatibleAllOf,
                    &member_pointer,
                    "`allOf` member is not object-compatible",
                );
                return Type::Unknown;
            };
            let Some(current) = merged.as_mut() else {
                merged = Some(table);
                continue;
            };
            if !merge_tables(current, table) {
                self.diagnostic(
                    DiagnosticCode::IncompatibleAllOf,
                    pointer,
                    "`allOf` members define conflicting object rules",
                );
                return Type::Unknown;
            }
        }
        merged.map_or(Type::Unknown, |(fields, indexer)| match indexer {
            Some(indexer) => Type::table_with_indexer(fields, indexer),
            None => Type::table(fields),
        })
    }

    fn lower_const(&mut self, value: &Value, pointer: &str) -> Type {
        match value {
            Value::String(value) => Type::Literal(value.clone().into()),
            Value::Bool(value) => Type::BooleanLiteral(*value),
            Value::Null => Type::Nil,
            Value::Number(_) => {
                self.widen(pointer, "numeric const widened to `number`", Type::Number)
            }
            Value::Array(_) => self.widen(
                pointer,
                "array const widened to `{unknown}`",
                Type::Unknown.array(),
            ),
            Value::Object(_) => self.widen(
                pointer,
                "object const widened to `{ [string]: unknown }`",
                Type::map(Type::String, Type::Unknown),
            ),
        }
    }

    fn lower_enum(&mut self, values: &[Value], pointer: &str) -> Type {
        if values.is_empty() {
            self.diagnostic(
                DiagnosticCode::UnsupportedSchema,
                pointer,
                "empty enum cannot be represented",
            );
            return Type::Unknown;
        }
        let mut widened = BTreeSet::new();
        let variants = values.iter().map(|value| match value {
            Value::String(value) => Type::Literal(value.clone().into()),
            Value::Bool(value) => Type::BooleanLiteral(*value),
            Value::Null => Type::Nil,
            Value::Number(_) => {
                widened.insert("number");
                Type::Number
            }
            Value::Array(_) => {
                widened.insert("array");
                Type::Unknown.array()
            }
            Value::Object(_) => {
                widened.insert("object");
                Type::map(Type::String, Type::Unknown)
            }
        });
        let result = schema_union(variants);
        for category in widened {
            self.diagnostic(
                DiagnosticCode::LiteralWidened,
                pointer,
                format!("{category} enum members widened to their Luau category"),
            );
        }
        result
    }

    fn widen(&mut self, pointer: &str, message: &str, ty: Type) -> Type {
        self.diagnostic(DiagnosticCode::LiteralWidened, pointer, message);
        ty
    }

    fn diagnostic(&mut self, code: DiagnosticCode, pointer: &str, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            code,
            pointer: pointer.to_owned(),
            message: message.into(),
        });
    }
}

const UNSUPPORTED_CONSTRAINTS: &[&str] = &[
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "maxItems",
    "minItems",
    "uniqueItems",
    "not",
    "patternProperties",
    "contains",
    "maxContains",
    "minContains",
    "maxProperties",
    "minProperties",
    "if",
    "then",
    "else",
    "dependentRequired",
    "dependentSchemas",
    "propertyNames",
    "unevaluatedProperties",
];

fn is_permissive_schema(schema: &Map<String, Value>) -> bool {
    !schema.keys().any(|key| {
        matches!(
            key.as_str(),
            "type"
                | "enum"
                | "const"
                | "oneOf"
                | "anyOf"
                | "allOf"
                | "$ref"
                | "properties"
                | "additionalProperties"
                | "items"
                | "prefixItems"
                | "required"
        ) || UNSUPPORTED_CONSTRAINTS.contains(&key.as_str())
    })
}

fn description(schema: &Map<String, Value>) -> Option<String> {
    schema
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn schema_union(types: impl IntoIterator<Item = Type>) -> Type {
    let mut normalized = Vec::new();
    for value in types {
        let values = match value {
            Type::Union(values) => values,
            value => vec![value],
        };
        for value in values {
            if value == Type::Unknown {
                return Type::Unknown;
            }
            if !normalized.contains(&value) {
                normalized.push(value);
            }
        }
    }
    Type::union(normalized)
}

fn table_parts(value: Type) -> Option<(Vec<Field>, Option<TableIndexer>)> {
    match value {
        Type::Table(fields) => Some((fields, None)),
        Type::TableWithIndexer { fields, indexer } => Some((fields, Some(indexer))),
        Type::Map(key, value) => Some((
            Vec::new(),
            Some(TableIndexer {
                key,
                value,
                read_only: false,
            }),
        )),
        _ => None,
    }
}

fn merge_tables(
    target: &mut (Vec<Field>, Option<TableIndexer>),
    source: (Vec<Field>, Option<TableIndexer>),
) -> bool {
    if target.1 != source.1 {
        return false;
    }
    let mut indexes = target
        .0
        .iter()
        .enumerate()
        .map(|(index, field)| (field.name.to_string(), index))
        .collect::<BTreeMap<_, _>>();
    for field in source.0 {
        if let Some(index) = indexes.get(field.name.as_ref()).copied() {
            let existing = &mut target.0[index];
            let Some(ty) = merge_field_types(&existing.ty, &field.ty) else {
                return false;
            };
            existing.ty = ty;
            if existing.doc.is_none() {
                existing.doc = field.doc;
            }
        } else {
            indexes.insert(field.name.to_string(), target.0.len());
            target.0.push(field);
        }
    }
    true
}

fn merge_field_types(left: &Type, right: &Type) -> Option<Type> {
    if left == right {
        return Some(left.clone());
    }
    match (left, right) {
        (Type::Optional(left), right) if left.as_ref() == right => Some(right.clone()),
        (left, Type::Optional(right)) if left == right.as_ref() => Some(left.clone()),
        _ => None,
    }
}

fn child(pointer: &str, value: &str) -> String {
    format!("{pointer}/{value}")
}

fn escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(any())]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn lowers_references_all_of_unions_literals_tuples_and_maps() {
        let schema = json!({
            "$defs": { "name": { "type": "string" } },
            "allOf": [
                { "type": "object", "additionalProperties": false,
                  "required": ["name"], "properties": { "name": { "$ref": "#/$defs/name" } } },
                { "type": "object", "additionalProperties": false,
                  "properties": { "choice": { "enum": ["a", "b"] },
                                  "tuple": { "type": "array", "prefixItems": [
                                      { "type": "number" }, { "type": "boolean" }
                                  ], "items": false } } }
            ]
        });
        let lowered = lower(&schema);
        assert!(lowered.diagnostics.is_empty(), "{:?}", lowered.diagnostics);
        let rendered = lowered.ty.render();
        assert!(rendered.contains("name: string"), "{rendered}");
        assert!(rendered.contains("choice: (\"a\" | \"b\")?"), "{rendered}");
        assert!(
            rendered.contains("tuple: {number | boolean}?"),
            "{rendered}"
        );
    }

    #[test]
    fn reports_cycles_external_refs_constraints_and_limits_without_panics() {
        for (schema, code) in [
            (
                json!({"$defs":{"x":{"$ref":"#/$defs/x"}},"$ref":"#/$defs/x"}),
                DiagnosticCode::ReferenceCycle,
            ),
            (
                json!({"$ref":"https://example.test/schema"}),
                DiagnosticCode::UnresolvedReference,
            ),
            (
                json!({"type":"string","pattern":"x"}),
                DiagnosticCode::UnsupportedSchema,
            ),
        ] {
            assert!(
                lower(&schema)
                    .diagnostics
                    .iter()
                    .any(|item| item.code == code)
            );
        }
        let limited = lower_with_limits(
            &json!({"type":"array","items":{"type":"array","items":{"type":"string"}}}),
            Limits {
                max_depth: 1,
                ..Limits::default()
            },
        );
        assert!(
            limited
                .diagnostics
                .iter()
                .any(|item| item.code == DiagnosticCode::LimitExceeded)
        );
    }

    #[test]
    fn constraints_widen_before_keyword_dispatch() {
        for schema in [
            json!({"type":["string"],"pattern":"x"}),
            json!({"$ref":"#/$defs/value","minimum":1,"$defs":{"value":{"type":"number"}}}),
        ] {
            let lowered = lower(&schema);
            assert_eq!(lowered.ty, Type::Unknown);
            assert_eq!(lowered.diagnostics.len(), 1);
            assert_eq!(
                lowered.diagnostics[0].code,
                DiagnosticCode::UnsupportedSchema
            );
        }
    }

    #[test]
    fn named_object_properties_do_not_imply_an_unknown_indexer() {
        let sealed = lower(&json!({
            "type": "object",
            "properties": {
                "timeout_milliseconds": { "type": "number" }
            }
        }));
        assert!(sealed.diagnostics.is_empty(), "{:?}", sealed.diagnostics);
        assert_eq!(
            sealed.ty,
            Type::table([Field::new("timeout_milliseconds", Type::Number.optional())])
        );

        let open = lower(&json!({
            "type": "object",
            "properties": {
                "timeout_milliseconds": { "type": "number" }
            },
            "additionalProperties": true
        }));
        assert_eq!(
            open.ty,
            Type::table_with_indexer(
                [Field::new("timeout_milliseconds", Type::Number.optional())],
                TableIndexer::new(Type::String, Type::Unknown),
            )
        );

        let mapped = lower(&json!({ "type": "object" }));
        assert_eq!(mapped.ty, Type::map(Type::String, Type::Unknown));

        let explicit = lower(&json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "additionalProperties": { "type": "number" }
        }));
        assert_eq!(
            explicit.ty,
            Type::table_with_indexer(
                [Field::new("name", Type::String.optional())],
                TableIndexer::new(Type::String, Type::Number),
            )
        );
    }

    #[test]
    fn prefix_items_include_the_additional_items_schema() {
        let lowered = lower(&json!({
            "type":"array",
            "prefixItems":[{"type":"number"}],
            "items":{"type":"string"}
        }));
        assert!(lowered.diagnostics.is_empty());
        assert_eq!(lowered.ty.render(), "{number | string}");

        let permissive = lower(&json!({
            "type":"array",
            "prefixItems":[{"type":"number"}]
        }));
        assert_eq!(permissive.ty, Type::Unknown.array());
    }

    #[test]
    fn boolean_and_adversarial_schemas_are_deterministic() {
        assert_eq!(lower(&Value::Bool(true)).ty, Type::Unknown);
        assert_eq!(lower(&Value::Bool(false)).diagnostics.len(), 1);
        let schema = json!({"type":"object","properties": {
            "a/b~c": {"const": 7}, "open": {}
        }});
        let first = lower(&schema);
        let second = lower(&schema);
        assert_eq!(first, second);
        assert_eq!(first.diagnostics[0].pointer, "/properties/a~1b~0c");
    }
}
