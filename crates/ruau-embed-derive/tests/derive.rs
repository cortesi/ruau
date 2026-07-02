#![allow(clippy::tests_outside_test_module)]
//! Integration tests for the Ruau embedding derive macros.

use ruau_embed_derive::{FromLua, IntoLua};
use ruau_vm::{Ambient, IntoLua as _, Limits, RuntimeCapabilities, ScopedValue, Vm};

#[derive(Debug, PartialEq, IntoLua, FromLua)]
struct Widget {
    name: String,
    count: i32,
    #[ruau(rename = "isActive")]
    active: bool,
    tags: Vec<String>,
    note: Option<String>,
}

#[derive(Debug, PartialEq, IntoLua, FromLua)]
struct Boxed<T> {
    value: T,
}

fn vm() -> Vm {
    Vm::builder()
        .ambient(Ambient::deterministic(0))
        .limits(Limits::unlimited())
        .runtime_capabilities(RuntimeCapabilities::default().enable_runtime_compilation())
        .trusted_host()
        .build()
        .expect("test VM builds")
}

#[test]
fn derives_named_struct_table_conversions() {
    let mut vm = vm();
    vm.step(|scope| {
        let widget = Widget {
            name: "alpha".to_string(),
            count: 7,
            active: true,
            tags: vec!["red".to_string(), "blue".to_string()],
            note: None,
        };
        let value = widget.into_lua(scope)?;
        let ScopedValue::Table(table) = value else {
            panic!("derived IntoLua materializes a table");
        };
        assert_eq!(table.get::<_, String>(scope, "name")?, "alpha");
        assert!(table.get::<_, bool>(scope, "isActive")?);

        let round_trip = <Widget as ruau_vm::FromLua>::from_lua(ScopedValue::Table(table), scope)?;
        assert_eq!(
            round_trip,
            Widget {
                name: "alpha".to_string(),
                count: 7,
                active: true,
                tags: vec!["red".to_string(), "blue".to_string()],
                note: None,
            }
        );
        Ok(())
    })
    .expect("derive round trip");
}

#[test]
fn derives_generic_struct_conversions() {
    let mut vm = vm();
    vm.step(|scope| {
        let value = Boxed { value: 99_i32 }.into_lua(scope)?;
        let boxed = <Boxed<i32> as ruau_vm::FromLua>::from_lua(value, scope)?;
        assert_eq!(boxed, Boxed { value: 99 });
        Ok(())
    })
    .expect("generic derive round trip");
}

#[test]
fn derived_from_lua_errors_include_field_paths() {
    let mut vm = vm();
    vm.step(|scope| {
        let table = scope.create_table()?;
        table.set(scope, "name", "alpha")?;
        table.set(scope, "count", 3_i32)?;
        table.set(scope, "isActive", true)?;
        table.set(scope, "tags", vec![1_i32, 2_i32])?;

        let err = <Widget as ruau_vm::FromLua>::from_lua(ScopedValue::Table(table), scope)
            .expect_err("bad nested field is rejected");
        assert_eq!(
            err.message(),
            "at .tags: at [1]: expected string, got number"
        );
        Ok(())
    })
    .expect("path error");
}

#[test]
fn derived_from_lua_rejects_wrong_typed_fields() {
    let mut vm = vm();
    vm.step(|scope| {
        let table = scope.create_table()?;
        table.set(scope, "name", "alpha")?;
        table.set(scope, "count", "three")?;
        table.set(scope, "isActive", true)?;
        table.set(scope, "tags", Vec::<String>::new())?;

        let err = <Widget as ruau_vm::FromLua>::from_lua(ScopedValue::Table(table), scope)
            .expect_err("wrong-typed field is rejected");
        assert_eq!(err.message(), "at .count: expected integer, got string");
        Ok(())
    })
    .expect("wrong-typed field error");
}

#[test]
fn derived_from_lua_rejects_missing_keys() {
    let mut vm = vm();
    vm.step(|scope| {
        let table = scope.create_table()?;
        table.set(scope, "count", 3_i32)?;
        table.set(scope, "isActive", true)?;
        table.set(scope, "tags", Vec::<String>::new())?;

        let err = <Widget as ruau_vm::FromLua>::from_lua(ScopedValue::Table(table), scope)
            .expect_err("missing key is rejected");
        assert_eq!(err.message(), "at .name: expected string, got nil");
        Ok(())
    })
    .expect("missing key error");
}

#[test]
fn derived_from_lua_rejects_non_table_values() {
    let mut vm = vm();
    vm.step(|scope| {
        let err = <Widget as ruau_vm::FromLua>::from_lua(ScopedValue::Nil, scope)
            .expect_err("non-table input is rejected");
        assert_eq!(err.message(), "expected table, got nil");
        Ok(())
    })
    .expect("non-table error");
}

#[test]
fn derived_from_lua_keeps_renamed_rust_key_absent() {
    let mut vm = vm();
    vm.step(|scope| {
        // The Rust field name `active` must not be consulted once the field is
        // renamed to `isActive`.
        let table = scope.create_table()?;
        table.set(scope, "name", "alpha")?;
        table.set(scope, "count", 3_i32)?;
        table.set(scope, "active", true)?;
        table.set(scope, "tags", Vec::<String>::new())?;

        let err = <Widget as ruau_vm::FromLua>::from_lua(ScopedValue::Table(table), scope)
            .expect_err("renamed field reads only the Lua key");
        assert_eq!(err.message(), "at .isActive: expected boolean, got nil");
        Ok(())
    })
    .expect("renamed key error");
}
