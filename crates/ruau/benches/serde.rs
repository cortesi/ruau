//! Serde benchmarks.

use std::{future::Future, time::Duration};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use ruau::{Luau, LuauString, Table as LuauTable, Value as LuauValue};
use tokio::runtime::Builder;

/// Run a future on a fresh current-thread Tokio runtime.
fn block_on<F: Future>(future: F) -> F::Output {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// Force two full garbage-collection cycles before a sample.
fn collect_gc_twice(lua: &Luau) {
    lua.gc_collect().unwrap();
    lua.gc_collect().unwrap();
}

/// Benchmark serializing Luau values to JSON.
fn encode_json(c: &mut Criterion) {
    let lua = Luau::new();

    let encode = lua
        .create_function(|_, t: LuauValue| Ok(serde_json::to_string(&t).unwrap()))
        .unwrap();
    let table = block_on(
        lua.load(
            r#"{
        name = "Clark Kent",
        address = {
            city = "Smallville",
            state = "Kansas",
            country = "USA",
        },
        age = 22,
        parents = {"Jonathan Kent", "Martha Kent"},
        superman = true,
        interests = {"flying", "saving the world", "kryptonite"},
    }"#,
        )
        .eval::<LuauTable>(),
    )
    .unwrap();

    c.bench_function("serialize json", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                block_on(encode.call::<LuauString>(&table)).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

/// Benchmark deserializing JSON to Luau values.
fn decode_json(c: &mut Criterion) {
    let lua = Luau::new();

    let decode = lua
        .create_function(|lua, s: String| {
            lua.to_value(&serde_json::from_str::<serde_json::Value>(&s).unwrap())
        })
        .unwrap();
    let json = r#"{
        "name": "Clark Kent",
        "address": {
            "city": "Smallville",
            "state": "Kansas",
            "country": "USA"
        },
        "age": 22,
        "parents": ["Jonathan Kent", "Martha Kent"],
        "superman": true,
        "interests": ["flying", "saving the world", "kryptonite"]
    }"#;

    c.bench_function("deserialize json", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                block_on(decode.call::<LuauTable>(json)).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(500)
        .measurement_time(Duration::from_secs(10))
        .noise_threshold(0.02);
    targets =
        encode_json,
        decode_json,
}

criterion_main!(benches);
