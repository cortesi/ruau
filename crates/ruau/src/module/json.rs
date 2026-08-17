//! Ready-made native JSON module.

use std::sync::Arc;

use ruau_declaration::DeclarationSource;
use ruau_vm::{
    HostArgCursor, IntoLuaMulti, MultiValue, NativeModule, RuntimeError, Scope, ScopedHostFunction,
    ScopedValue, Table, TableLayout,
    serde::{json_null_module_value, json_to_scoped_value, mark_json_array, scoped_value_to_json},
};

use super::{Binding, Builder};

/// Native module name used by [`native_module`].
pub const MODULE_NAME: &str = "json";
/// Public declaration installed with the native module.
pub const PUBLIC_DECLARATION: &str = include_str!("json.d.luau");

/// Builds the ready-made native JSON module.
#[must_use]
pub fn native_module() -> Arc<dyn NativeModule> {
    let mut builder =
        Builder::from_declaration(MODULE_NAME, DeclarationSource::Text(PUBLIC_DECLARATION));
    builder.constant(
        "null",
        Binding::declared_library(MODULE_NAME),
        json_null_module_value(),
    );
    for (name, function) in [
        (
            "deserialize",
            Arc::new(JsonDeserialize) as Arc<dyn ScopedHostFunction>,
        ),
        (
            "serialize",
            Arc::new(JsonSerialize) as Arc<dyn ScopedHostFunction>,
        ),
        (
            "object",
            Arc::new(JsonObject) as Arc<dyn ScopedHostFunction>,
        ),
        ("array", Arc::new(JsonArray) as Arc<dyn ScopedHostFunction>),
        (
            "as_object",
            Arc::new(JsonAsObject) as Arc<dyn ScopedHostFunction>,
        ),
        (
            "as_array",
            Arc::new(JsonAsArray) as Arc<dyn ScopedHostFunction>,
        ),
        ("get", Arc::new(JsonGet) as Arc<dyn ScopedHostFunction>),
        (
            "get_string",
            Arc::new(JsonGetTyped::new(is_string)) as Arc<dyn ScopedHostFunction>,
        ),
        (
            "get_number",
            Arc::new(JsonGetTyped::new(is_number)) as Arc<dyn ScopedHostFunction>,
        ),
        (
            "get_boolean",
            Arc::new(JsonGetTyped::new(is_boolean)) as Arc<dyn ScopedHostFunction>,
        ),
        (
            "get_array",
            Arc::new(JsonGetTyped::new(is_array)) as Arc<dyn ScopedHostFunction>,
        ),
        (
            "get_object",
            Arc::new(JsonGetTyped::new(is_object)) as Arc<dyn ScopedHostFunction>,
        ),
    ] {
        builder.scoped_function(name, Binding::declared_library(MODULE_NAME), function);
    }
    builder.build().expect("JSON module declaration validates")
}

struct JsonDeserialize;

impl ScopedHostFunction for JsonDeserialize {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        values: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let mut args = HostArgCursor::new(scope, values);
        let source = args.required::<String>("src")?;
        args.finish()?;
        let value = serde_json::from_str(&source)
            .map_err(|error| json_error("parse_error", error.to_string()))?;
        Ok(MultiValue::from_values(vec![json_to_scoped_value(
            scope, &value,
        )?]))
    }
}

struct JsonSerialize;

impl ScopedHostFunction for JsonSerialize {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        values: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let mut args = HostArgCursor::new(scope, values);
        let value = args.required::<ScopedValue<'s>>("value")?;
        let pretty = args.defaulted::<bool>("pretty_print")?;
        args.finish()?;
        let json = scoped_value_to_json(scope, value)
            .map_err(|error| json_error("serialize_error", error.to_string()))?;
        let text = if pretty {
            serde_json::to_string_pretty(&json)
        } else {
            serde_json::to_string(&json)
        }
        .map_err(|error| json_error("serialize_error", error.to_string()))?;
        text.into_lua_multi(scope)
    }
}

struct JsonObject;
struct JsonArray;

impl ScopedHostFunction for JsonObject {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        values: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let mut args = HostArgCursor::new(scope, values);
        let table = args.required::<Table<'s>>("props")?;
        args.finish()?;
        if ruau_vm::serde::is_json_array_table(scope, table)? {
            return Err(RuntimeError::runtime(
                "a marked JSON array cannot be used as a JSON object",
            ));
        }
        match table.layout(scope)? {
            TableLayout::Empty | TableLayout::StringMap { .. } => {
                Ok(MultiValue::from_values(vec![ScopedValue::Table(table)]))
            }
            _ => Err(RuntimeError::runtime(
                "JSON object tables may only have string keys",
            )),
        }
    }
}

impl ScopedHostFunction for JsonArray {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        values: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let mut args = HostArgCursor::new(scope, values);
        let table = match args.optional::<Table<'s>>("t")? {
            Some(table) => table,
            None => scope.create_table()?,
        };
        args.finish()?;
        match table.layout(scope)? {
            TableLayout::Empty | TableLayout::Sequence { .. } => {
                mark_json_array(scope, table)?;
                Ok(MultiValue::from_values(vec![ScopedValue::Table(table)]))
            }
            _ => Err(RuntimeError::runtime(
                "JSON array tables must be dense and 1-based",
            )),
        }
    }
}

struct JsonAsObject;
struct JsonAsArray;

impl ScopedHostFunction for JsonAsObject {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        values: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        classify_argument(scope, values, |shape| shape == JsonTableShape::Object)
    }
}

impl ScopedHostFunction for JsonAsArray {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        values: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        classify_argument(scope, values, |shape| shape == JsonTableShape::Array)
    }
}

fn classify_argument<'s>(
    scope: &Scope<'s>,
    values: MultiValue<'s>,
    accept: impl FnOnce(JsonTableShape) -> bool,
) -> Result<MultiValue<'s>, RuntimeError> {
    let mut args = HostArgCursor::new(scope, values);
    let value = args.raw().unwrap_or(ScopedValue::Nil);
    args.finish()?;
    let accepted = match value {
        ScopedValue::Table(table) => classify_table(scope, table).is_ok_and(accept),
        _ => false,
    };
    Ok(MultiValue::from_values(vec![if accepted {
        value
    } else {
        ScopedValue::Nil
    }]))
}

struct JsonGet;

impl ScopedHostFunction for JsonGet {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        values: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let mut args = HostArgCursor::new(scope, values);
        let value = args.raw().unwrap_or(ScopedValue::Nil);
        let path = args.required::<Table<'s>>("path")?;
        args.finish()?;
        Ok(MultiValue::from_values(vec![json_get(scope, value, path)?]))
    }
}

struct JsonGetTyped {
    check: for<'s> fn(&Scope<'s>, ScopedValue<'s>) -> Result<bool, RuntimeError>,
}

impl JsonGetTyped {
    const fn new(
        check: for<'s> fn(&Scope<'s>, ScopedValue<'s>) -> Result<bool, RuntimeError>,
    ) -> Self {
        Self { check }
    }
}

impl ScopedHostFunction for JsonGetTyped {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        values: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let mut args = HostArgCursor::new(scope, values);
        let value = args.raw().unwrap_or(ScopedValue::Nil);
        let path = args.required::<Table<'s>>("path")?;
        args.finish()?;
        let value = json_get(scope, value, path)?;
        Ok(MultiValue::from_values(vec![
            if (self.check)(scope, value)? {
                value
            } else {
                ScopedValue::Nil
            },
        ]))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonTableShape {
    Array,
    Object,
}

fn classify_table<'s>(scope: &Scope<'s>, table: Table<'s>) -> Result<JsonTableShape, RuntimeError> {
    let layout = table.json_layout(scope)?;
    match (layout.layout, layout.marked_array) {
        (TableLayout::Empty, true) | (TableLayout::Sequence { .. }, _) => Ok(JsonTableShape::Array),
        (TableLayout::Empty | TableLayout::StringMap { .. }, false) => Ok(JsonTableShape::Object),
        (TableLayout::StringMap { .. }, true) | (TableLayout::Mixed { .. }, _) => Err(
            RuntimeError::runtime("JSON tables cannot mix array and object keys"),
        ),
        (TableLayout::Sparse { .. }, _) => Err(RuntimeError::runtime(
            "JSON arrays must be dense and 1-based",
        )),
        (TableLayout::UnsupportedKey { key }, _) => Err(RuntimeError::runtime(format!(
            "unsupported JSON table key `{key:?}`"
        ))),
    }
}

fn json_get<'s>(
    scope: &Scope<'s>,
    mut value: ScopedValue<'s>,
    path: Table<'s>,
) -> Result<ScopedValue<'s>, RuntimeError> {
    let segment_count = match path.json_layout(scope)?.layout {
        TableLayout::Empty => return Ok(value),
        TableLayout::Sequence { len } => len,
        TableLayout::StringMap { .. } | TableLayout::Mixed { .. } => {
            return Err(RuntimeError::runtime(
                "JSON paths must be arrays of string or positive integer segments",
            ));
        }
        TableLayout::Sparse { .. } => {
            return Err(RuntimeError::runtime(
                "JSON path arrays must be dense and 1-based",
            ));
        }
        TableLayout::UnsupportedKey { key } => {
            return Err(RuntimeError::runtime(format!(
                "unsupported JSON path key `{key:?}`"
            )));
        }
    };
    for index in 1..=segment_count {
        #[expect(
            clippy::cast_precision_loss,
            reason = "table sequence lengths cannot reach f64's integer precision limit"
        )]
        let segment: ScopedValue<'_> = path.get(scope, index as f64)?;
        let ScopedValue::Table(table) = value else {
            return Ok(ScopedValue::Nil);
        };
        value = match segment {
            ScopedValue::String(key) => table.get(scope, ScopedValue::String(key))?,
            ScopedValue::Integer(index) if index > 0 => table.get(scope, index)?,
            ScopedValue::Number(index) if index > 0.0 && index.fract() == 0.0 => {
                table.get(scope, index)?
            }
            _ => {
                return Err(RuntimeError::runtime(
                    "JSON path segments must be strings or positive integers",
                ));
            }
        };
        if matches!(value, ScopedValue::Nil) {
            return Ok(value);
        }
    }
    Ok(value)
}

fn is_string<'s>(_scope: &Scope<'s>, value: ScopedValue<'s>) -> Result<bool, RuntimeError> {
    Ok(matches!(value, ScopedValue::String(_)))
}

fn is_number<'s>(_scope: &Scope<'s>, value: ScopedValue<'s>) -> Result<bool, RuntimeError> {
    Ok(matches!(
        value,
        ScopedValue::Integer(_) | ScopedValue::Number(_)
    ))
}

fn is_boolean<'s>(_scope: &Scope<'s>, value: ScopedValue<'s>) -> Result<bool, RuntimeError> {
    Ok(matches!(value, ScopedValue::Boolean(_)))
}

fn is_array<'s>(scope: &Scope<'s>, value: ScopedValue<'s>) -> Result<bool, RuntimeError> {
    match value {
        ScopedValue::Table(table) => {
            Ok(classify_table(scope, table).is_ok_and(|shape| shape == JsonTableShape::Array))
        }
        _ => Ok(false),
    }
}

fn is_object<'s>(scope: &Scope<'s>, value: ScopedValue<'s>) -> Result<bool, RuntimeError> {
    match value {
        ScopedValue::Table(table) => {
            Ok(classify_table(scope, table).is_ok_and(|shape| shape == JsonTableShape::Object))
        }
        _ => Ok(false),
    }
}

fn json_error(kind: &'static str, message: String) -> RuntimeError {
    RuntimeError::runtime(&message)
        .with_script_field("module", MODULE_NAME)
        .with_script_field("kind", kind)
        .with_script_field("message", message)
}
