use std::{collections::HashSet, os::raw::c_void};

use super::{outbound::MAX_VISIT_DEPTH, *};
use crate::{Buffer, Integer, Luau, LuauString, Number, Result, Table, Value};

#[derive(Debug, PartialEq)]
enum Seen {
    Nil,
    Bool(bool),
    Int(Integer),
    Number(Number),
    String(Vec<u8>),
    Buffer(Vec<u8>),
    Array(Vec<Self>),
    Map(Vec<(String, Self)>),
    Host(String),
}

struct RecordingVisitor {
    paths: Vec<String>,
    handled_tables: HashSet<*const c_void>,
}

impl RecordingVisitor {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            handled_tables: HashSet::new(),
        }
    }

    fn with_handled_table(table: &Table) -> Self {
        let mut visitor = Self::new();
        visitor.handled_tables.insert(table.to_pointer());
        visitor
    }

    fn record(&mut self, path: &ValuePath) {
        self.paths.push(path.to_string());
    }
}

impl OutboundVisitor for RecordingVisitor {
    type Output = Seen;

    fn nil(&mut self, path: &ValuePath) -> ValueVisitResult<Self::Output> {
        self.record(path);
        Ok(Seen::Nil)
    }

    fn boolean(&mut self, value: bool, path: &ValuePath) -> ValueVisitResult<Self::Output> {
        self.record(path);
        Ok(Seen::Bool(value))
    }

    fn integer(&mut self, value: Integer, path: &ValuePath) -> ValueVisitResult<Self::Output> {
        self.record(path);
        Ok(Seen::Int(value))
    }

    fn number(&mut self, value: Number, path: &ValuePath) -> ValueVisitResult<Self::Output> {
        self.record(path);
        Ok(Seen::Number(value))
    }

    fn string(&mut self, value: &LuauString, path: &ValuePath) -> ValueVisitResult<Self::Output> {
        self.record(path);
        Ok(Seen::String(value.as_bytes().to_vec()))
    }

    fn buffer(&mut self, value: &Buffer, path: &ValuePath) -> ValueVisitResult<Self::Output> {
        self.record(path);
        Ok(Seen::Buffer(value.to_vec()))
    }

    fn table(
        &mut self,
        table: &Table,
        path: &ValuePath,
    ) -> ValueVisitResult<BoundaryAction<Self::Output>> {
        if self.handled_tables.contains(&table.to_pointer()) {
            self.record(path);
            Ok(BoundaryAction::Replace(Seen::Host(path.to_string())))
        } else {
            Ok(BoundaryAction::Descend)
        }
    }

    fn array(
        &mut self,
        values: Vec<Self::Output>,
        path: &ValuePath,
    ) -> ValueVisitResult<Self::Output> {
        self.record(path);
        Ok(Seen::Array(values))
    }

    fn map(
        &mut self,
        entries: Vec<(String, Self::Output)>,
        path: &ValuePath,
    ) -> ValueVisitResult<Self::Output> {
        self.record(path);
        Ok(Seen::Map(entries))
    }
}

#[test]
fn outbound_tracks_paths_through_arrays_and_maps() -> Result<()> {
    let lua = Luau::new();
    let root = lua.create_table()?;
    let foo = lua.create_table()?;
    foo.raw_set(1, true)?;
    foo.raw_set(2, 7)?;
    root.raw_set("foo", foo)?;

    let mut visitor = RecordingVisitor::new();
    let output =
        visit_luau_value_at_path(&Value::Table(root), ValuePath::argument(1), &mut visitor)
            .expect("visit should succeed");

    assert_eq!(
        output,
        Seen::Map(vec![(
            "foo".to_string(),
            Seen::Array(vec![Seen::Bool(true), Seen::Int(7)])
        )])
    );
    assert_eq!(
        visitor.paths,
        [
            "argument 1.foo[1]",
            "argument 1.foo[2]",
            "argument 1.foo",
            "argument 1"
        ]
    );
    Ok(())
}

#[test]
fn outbound_reports_sparse_array_path() -> Result<()> {
    let lua = Luau::new();
    let table = lua.create_table()?;
    table.raw_set(1, "first")?;
    table.raw_set(3, "third")?;

    let mut visitor = RecordingVisitor::new();
    let error =
        visit_luau_value(&Value::Table(table), &mut visitor).expect_err("sparse array should fail");

    assert!(matches!(
        error,
        ValueVisitError::SparseArray { index: 2, .. }
    ));
    assert_eq!(error.path().to_string(), "value");
    Ok(())
}

#[test]
fn outbound_reports_mixed_table_path() -> Result<()> {
    let lua = Luau::new();
    let table = lua.create_table()?;
    table.raw_set(1, "first")?;
    table.raw_set("name", "second")?;

    let mut visitor = RecordingVisitor::new();
    let error =
        visit_luau_value(&Value::Table(table), &mut visitor).expect_err("mixed table should fail");

    assert!(matches!(error, ValueVisitError::MixedTableKeys { .. }));
    assert_eq!(error.path().to_string(), "value");
    Ok(())
}

#[test]
fn outbound_detects_table_cycles_after_table_hook() -> Result<()> {
    let lua = Luau::new();
    let table = lua.create_table()?;
    table.raw_set("self", table.clone())?;

    let mut visitor = RecordingVisitor::new();
    let error = visit_luau_value(&Value::Table(table.clone()), &mut visitor)
        .expect_err("cycle should fail");

    assert!(matches!(error, ValueVisitError::Cycle { .. }));
    assert_eq!(error.path().to_string(), "value.self");

    let mut visitor = RecordingVisitor::with_handled_table(&table);
    let output = visit_luau_value(&Value::Table(table), &mut visitor)
        .expect("table hook should short-circuit cycle detection");
    assert_eq!(output, Seen::Host("value".to_string()));
    Ok(())
}

#[test]
fn outbound_rejects_deep_acyclic_tables() -> Result<()> {
    let lua = Luau::new();
    let root = lua.create_table()?;
    let mut current = root.clone();
    for _ in 0..=MAX_VISIT_DEPTH {
        let child = lua.create_table()?;
        current.raw_set(1, child.clone())?;
        current = child;
    }

    let mut visitor = RecordingVisitor::new();
    let error =
        visit_luau_value(&Value::Table(root), &mut visitor).expect_err("deep nesting should fail");
    assert!(matches!(
        error,
        ValueVisitError::DepthLimit {
            max_depth: MAX_VISIT_DEPTH,
            ..
        }
    ));
    Ok(())
}

#[test]
fn outbound_host_value_hook_can_replace_userdata() -> Result<()> {
    #[derive(Clone)]
    struct Handle;

    impl crate::UserData for Handle {}

    struct HandleVisitor;

    impl OutboundVisitor for HandleVisitor {
        type Output = String;

        fn nil(&mut self, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
            Ok("nil".to_string())
        }

        fn boolean(&mut self, _value: bool, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
            Ok("boolean".to_string())
        }

        fn integer(
            &mut self,
            _value: Integer,
            _path: &ValuePath,
        ) -> ValueVisitResult<Self::Output> {
            Ok("integer".to_string())
        }

        fn number(&mut self, _value: Number, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
            Ok("number".to_string())
        }

        fn string(
            &mut self,
            _value: &LuauString,
            _path: &ValuePath,
        ) -> ValueVisitResult<Self::Output> {
            Ok("string".to_string())
        }

        fn buffer(&mut self, _value: &Buffer, _path: &ValuePath) -> ValueVisitResult<Self::Output> {
            Ok("buffer".to_string())
        }

        fn host_value(
            &mut self,
            value: HostValue<'_>,
            path: &ValuePath,
        ) -> ValueVisitResult<BoundaryAction<Self::Output>> {
            assert!(matches!(value, HostValue::UserData(_)));
            Ok(BoundaryAction::Replace(format!("handle:{path}")))
        }

        fn array(
            &mut self,
            _values: Vec<Self::Output>,
            _path: &ValuePath,
        ) -> ValueVisitResult<Self::Output> {
            Ok("array".to_string())
        }

        fn map(
            &mut self,
            _entries: Vec<(String, Self::Output)>,
            _path: &ValuePath,
        ) -> ValueVisitResult<Self::Output> {
            Ok("map".to_string())
        }
    }

    let lua = Luau::new();
    let userdata = lua.create_userdata(Handle)?;
    let output = visit_luau_value(&Value::UserData(userdata), &mut HandleVisitor)
        .expect("host hook should replace userdata");

    assert_eq!(output, "handle:value");
    Ok(())
}

enum Source {
    Nil,
    Int(Integer),
    Text(String),
    Binary(Vec<u8>),
    Array(Vec<Self>),
    Map(Vec<(InboundKey, Self)>),
}

enum InboundKey {
    String(String),
    Unsupported(&'static str),
}

impl InboundSource for Source {
    fn inbound_kind(&self, _path: &ValuePath) -> ValueVisitResult<InboundKind<'_, Self>> {
        Ok(match self {
            Self::Nil => InboundKind::Nil,
            Self::Int(value) => InboundKind::Integer(*value),
            Self::Text(value) => InboundKind::String(value),
            Self::Binary(value) => InboundKind::Binary(value),
            Self::Array(values) => InboundKind::Array(values.iter().collect()),
            Self::Map(entries) => InboundKind::Map(
                entries
                    .iter()
                    .map(|(key, value)| {
                        let key = match key {
                            InboundKey::String(key) => InboundMapKey::String(key),
                            InboundKey::Unsupported(type_name) => {
                                InboundMapKey::Unsupported(type_name)
                            }
                        };
                        (key, value)
                    })
                    .collect(),
            ),
        })
    }
}

#[test]
fn inbound_converts_arrays_maps_and_binary() -> Result<()> {
    let lua = Luau::new();
    let source = Source::Map(vec![
        (
            InboundKey::String("items".to_string()),
            Source::Array(vec![Source::Int(1), Source::Text("two".to_string())]),
        ),
        (
            InboundKey::String("payload".to_string()),
            Source::Binary(vec![1, 2, 3]),
        ),
    ]);

    let value = inbound_to_luau(&lua, &source, &mut DefaultInboundVisitor)
        .expect("inbound conversion should succeed");

    let Value::Table(table) = value else {
        panic!("expected table");
    };
    let items: Table = table.raw_get("items")?;
    assert_eq!(items.raw_get::<Integer>(1)?, 1);
    assert_eq!(items.raw_get::<String>(2)?, "two");
    let payload: Buffer = table.raw_get("payload")?;
    assert_eq!(payload.to_vec(), [1, 2, 3]);
    Ok(())
}

#[test]
fn inbound_map_hook_runs_before_generic_conversion() -> Result<()> {
    struct RefVisitor;

    impl InboundVisitor<Source> for RefVisitor {
        fn map(
            &mut self,
            entries: &[(InboundMapKey<'_>, &Source)],
            _lua: &Luau,
            path: &ValuePath,
        ) -> ValueVisitResult<BoundaryAction<Value>> {
            if entries.len() == 1
                && let (InboundMapKey::String("$ref"), Source::Text(reference)) = entries[0]
            {
                return Ok(BoundaryAction::Replace(Value::String(
                    _lua.create_string(format!("ref:{path}:{reference}"))
                        .map_err(|error| ValueVisitError::luau(path, error))?,
                )));
            }
            Ok(BoundaryAction::Descend)
        }
    }

    let lua = Luau::new();
    let source = Source::Map(vec![(
        InboundKey::String("$ref".to_string()),
        Source::Text("handle".to_string()),
    )]);

    let value =
        inbound_to_luau(&lua, &source, &mut RefVisitor).expect("map hook should replace value");
    let Value::String(value) = value else {
        panic!("expected string");
    };
    assert_eq!(value.to_str()?, "ref:value:handle");
    Ok(())
}

#[test]
fn inbound_reports_unsupported_key_path() {
    let lua = Luau::new();
    let source = Source::Map(vec![(InboundKey::Unsupported("integer"), Source::Nil)]);

    let error = inbound_to_luau(&lua, &source, &mut DefaultInboundVisitor)
        .expect_err("unsupported key should fail");

    assert!(matches!(error, ValueVisitError::UnsupportedTableKey { .. }));
    assert_eq!(error.path().to_string(), "value");
}

#[test]
fn inbound_rejects_deep_sources() {
    let lua = Luau::new();
    let mut source = Source::Nil;
    for _ in 0..=MAX_VISIT_DEPTH {
        source = Source::Array(vec![source]);
    }

    let error = inbound_to_luau(&lua, &source, &mut DefaultInboundVisitor)
        .expect_err("deep nesting should fail");

    assert!(matches!(
        error,
        ValueVisitError::DepthLimit {
            max_depth: MAX_VISIT_DEPTH,
            ..
        }
    ));
}
