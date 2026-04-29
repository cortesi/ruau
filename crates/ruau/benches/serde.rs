#![allow(
    missing_docs,
    clippy::absolute_paths,
    clippy::missing_docs_in_private_items,
    clippy::tests_outside_test_module,
    clippy::items_after_statements,
    clippy::cognitive_complexity,
    clippy::let_underscore_must_use,
    clippy::manual_c_str_literals,
    clippy::mutable_key_type,
    clippy::needless_maybe_sized,
    clippy::needless_pass_by_value,
    clippy::redundant_pattern_matching
)]

use std::{future::Future, time::Duration};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use ruau::prelude::*;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn collect_gc_twice(lua: &Lua) {
    lua.gc_collect().unwrap();
    lua.gc_collect().unwrap();
}

fn encode_json(c: &mut Criterion) {
    let lua = Lua::new();

    let encode = lua
        .create_function(|_, t: LuaValue| Ok(serde_json::to_string(&t).unwrap()))
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
        .eval::<LuaTable>(),
    )
    .unwrap();

    c.bench_function("serialize json", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                block_on(encode.call::<LuaString>(&table)).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

fn decode_json(c: &mut Criterion) {
    let lua = Lua::new();

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
                block_on(decode.call::<LuaTable>(json)).unwrap();
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
