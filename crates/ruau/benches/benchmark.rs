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

use std::{
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use ruau::prelude::*;
use tokio::{runtime::Runtime, task};

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn collect_gc_twice(lua: &Luau) {
    lua.gc_collect().unwrap();
    lua.gc_collect().unwrap();
}

fn table_create_empty(c: &mut Criterion) {
    let lua = Luau::new();

    c.bench_function("table [create empty]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                lua.create_table().unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

fn table_create_array(c: &mut Criterion) {
    let lua = Luau::new();

    c.bench_function("table [create array]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                lua.create_sequence_from(1..=10).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

fn table_create_hash(c: &mut Criterion) {
    let lua = Luau::new();

    c.bench_function("table [create hash]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                lua.create_table_from(
                    ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]
                        .into_iter()
                        .map(|s| (s, s)),
                )
                .unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

fn table_get_set(c: &mut Criterion) {
    let lua = Luau::new();

    c.bench_function("table [get and set]", |b| {
        b.iter_batched(
            || {
                collect_gc_twice(&lua);
                lua.create_table().unwrap()
            },
            |table| {
                for (i, s) in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]
                    .into_iter()
                    .enumerate()
                {
                    table.raw_set(s, i).unwrap();
                    assert_eq!(table.raw_get::<usize>(s).unwrap(), i);
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn table_traversal_pairs(c: &mut Criterion) {
    let lua = Luau::new();

    c.bench_function("table [traversal pairs]", |b| {
        b.iter_batched(
            || lua.globals(),
            |globals| {
                for kv in globals.pairs::<String, LuauValue>() {
                    let (_k, _v) = kv.unwrap();
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn table_traversal_for_each(c: &mut Criterion) {
    let lua = Luau::new();

    c.bench_function("table [traversal for_each]", |b| {
        b.iter_batched(
            || lua.globals(),
            |globals| globals.for_each::<String, LuauValue>(|_k, _v| Ok(())),
            BatchSize::SmallInput,
        );
    });
}

fn table_traversal_sequence(c: &mut Criterion) {
    let lua = Luau::new();

    let table = lua.create_sequence_from(1..1000).unwrap();

    c.bench_function("table [traversal sequence]", |b| {
        b.iter_batched(
            || table.clone(),
            |table| {
                for v in table.sequence_values::<i32>() {
                    let _i = v.unwrap();
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn table_ref_clone(c: &mut Criterion) {
    let lua = Luau::new();

    let t = lua.create_table().unwrap();

    c.bench_function("table [ref clone]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                let _t2 = t.clone();
            },
            BatchSize::SmallInput,
        );
    });
}

fn function_create(c: &mut Criterion) {
    let lua = Luau::new();

    c.bench_function("function [create Rust]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                lua.create_function(|_, ()| Ok(123)).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

fn function_call_sum(c: &mut Criterion) {
    let lua = Luau::new();

    let sum = lua
        .create_function(|_, (a, b, c): (i64, i64, i64)| Ok(a + b - c))
        .unwrap();

    c.bench_function("function [call Rust sum]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                assert_eq!(block_on(sum.call::<i64>((10, 20, 30))).unwrap(), 0);
            },
            BatchSize::SmallInput,
        );
    });
}

fn function_call_lua_sum(c: &mut Criterion) {
    let lua = Luau::new();

    let sum = block_on(
        lua.load("function(a, b, c) return a + b - c end")
            .eval::<LuauFunction>(),
    )
    .unwrap();

    c.bench_function("function [call Luau sum]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                assert_eq!(block_on(sum.call::<i64>((10, 20, 30))).unwrap(), 0);
            },
            BatchSize::SmallInput,
        );
    });
}

fn function_call_concat(c: &mut Criterion) {
    let lua = Luau::new();

    let concat = lua
        .create_function(|_, (a, b): (LuauString, LuauString)| {
            Ok(format!("{}{}", a.to_str()?, b.to_str()?))
        })
        .unwrap();
    let i = AtomicUsize::new(0);

    c.bench_function("function [call Rust concat string]", |b| {
        b.iter_batched(
            || {
                collect_gc_twice(&lua);
                i.fetch_add(1, Ordering::Relaxed)
            },
            |i| {
                assert_eq!(
                    block_on(concat.call::<LuauString>(("num:", i))).unwrap(),
                    format!("num:{i}")
                );
            },
            BatchSize::SmallInput,
        );
    });
}

fn function_call_lua_concat(c: &mut Criterion) {
    let lua = Luau::new();

    let concat = block_on(
        lua.load("function(a, b) return a..b end")
            .eval::<LuauFunction>(),
    )
    .unwrap();
    let i = AtomicUsize::new(0);

    c.bench_function("function [call Luau concat string]", |b| {
        b.iter_batched(
            || {
                collect_gc_twice(&lua);
                i.fetch_add(1, Ordering::Relaxed)
            },
            |i| {
                assert_eq!(
                    block_on(concat.call::<LuauString>(("num:", i))).unwrap(),
                    format!("num:{i}")
                );
            },
            BatchSize::SmallInput,
        );
    });
}

fn function_async_call_sum(c: &mut Criterion) {
    let options = LuauOptions::new().thread_pool_size(1024);
    let lua = Luau::new_with(LuauStdLib::ALL_SAFE, options).unwrap();

    let sum = lua
        .create_async_function(async |_, (a, b, c): (i64, i64, i64)| {
            task::yield_now().await;
            Ok(a + b - c)
        })
        .unwrap();

    c.bench_function("function [async call Rust sum]", |b| {
        let rt = Runtime::new().unwrap();
        b.to_async(rt).iter_batched(
            || collect_gc_twice(&lua),
            |_| async {
                assert_eq!(sum.call::<i64>((10, 20, 30)).await.unwrap(), 0);
            },
            BatchSize::SmallInput,
        );
    });
}

fn registry_value_create(c: &mut Criterion) {
    let lua = Luau::new();
    lua.gc_stop();

    c.bench_function("registry value [create]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| lua.create_registry_value("hello").unwrap(),
            BatchSize::SmallInput,
        );
    });
}

fn registry_value_get(c: &mut Criterion) {
    let lua = Luau::new();
    lua.gc_stop();

    let value = lua.create_registry_value("hello").unwrap();

    c.bench_function("registry value [get]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                assert_eq!(lua.registry_value::<LuauString>(&value).unwrap(), "hello");
            },
            BatchSize::SmallInput,
        );
    });
}

fn userdata_create(c: &mut Criterion) {
    struct UserData(#[allow(unused)] i64);
    impl LuauUserData for UserData {}

    let lua = Luau::new();

    c.bench_function("userdata [create]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                lua.create_userdata(UserData(123)).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

fn userdata_call_index(c: &mut Criterion) {
    struct UserData(#[allow(unused)] i64);
    impl LuauUserData for UserData {
        fn add_methods<M: LuauUserDataMethods<Self>>(methods: &mut M) {
            methods.add_meta_method(LuauMetaMethod::Index, move |_, _, key: LuauString| Ok(key));
        }
    }

    let lua = Luau::new();
    let ud = lua.create_userdata(UserData(123)).unwrap();
    let index = block_on(
        lua.load("function(ud) return ud.test end")
            .eval::<LuauFunction>(),
    )
    .unwrap();

    c.bench_function("userdata [call index]", |b| {
        b.iter_batched(
            || collect_gc_twice(&lua),
            |_| {
                assert_eq!(block_on(index.call::<LuauString>(&ud)).unwrap(), "test");
            },
            BatchSize::SmallInput,
        );
    });
}

fn userdata_call_method(c: &mut Criterion) {
    struct UserData(i64);
    impl LuauUserData for UserData {
        fn add_methods<M: LuauUserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("add", |_, this, i: i64| Ok(this.0 + i));
        }
    }

    let lua = Luau::new();
    let ud = lua.create_userdata(UserData(123)).unwrap();
    let method = block_on(
        lua.load("function(ud, i) return ud:add(i) end")
            .eval::<LuauFunction>(),
    )
    .unwrap();
    let i = AtomicUsize::new(0);

    c.bench_function("userdata [call method]", |b| {
        b.iter_batched(
            || {
                collect_gc_twice(&lua);
                i.fetch_add(1, Ordering::Relaxed)
            },
            |i| {
                assert_eq!(block_on(method.call::<usize>((&ud, i))).unwrap(), 123 + i);
            },
            BatchSize::SmallInput,
        );
    });
}

// A userdata method call that goes through an implicit `__index` function
fn userdata_call_method_complex(c: &mut Criterion) {
    struct UserData(u64);
    impl LuauUserData for UserData {
        fn register(registry: &mut LuauUserDataRegistry<Self>) {
            registry.add_field_method_get("val", |_, this| Ok(this.0));
            registry.add_method_mut("inc_by", |_, this, by: u64| {
                this.0 += by;
                Ok(this.0)
            });
            registry.enable_namecall();
        }
    }

    let lua = Luau::new();
    let ud = lua.create_userdata(UserData(0)).unwrap();
    let inc_by = block_on(
        lua.load("function(ud, s) return ud:inc_by(s) end")
            .eval::<LuauFunction>(),
    )
    .unwrap();

    c.bench_function("userdata [call method complex]", |b| {
        b.iter_batched(
            || {
                collect_gc_twice(&lua);
            },
            |_| {
                block_on(inc_by.call::<()>((&ud, 1))).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

fn userdata_async_call_method(c: &mut Criterion) {
    struct UserData(i64);
    impl LuauUserData for UserData {
        fn add_methods<M: LuauUserDataMethods<Self>>(methods: &mut M) {
            methods.add_async_method("add", async |_, this, i: i64| {
                task::yield_now().await;
                Ok(this.0 + i)
            });
        }
    }

    let options = LuauOptions::new().thread_pool_size(1024);
    let lua = Luau::new_with(LuauStdLib::ALL_SAFE, options).unwrap();
    let ud = lua.create_userdata(UserData(123)).unwrap();
    let method = block_on(
        lua.load("function(ud, i) return ud:add(i) end")
            .eval::<LuauFunction>(),
    )
    .unwrap();
    let i = AtomicUsize::new(0);

    c.bench_function("userdata [async call method] 10", |b| {
        let rt = Runtime::new().unwrap();
        b.to_async(rt).iter_batched(
            || {
                collect_gc_twice(&lua);
                (
                    method.clone(),
                    ud.clone(),
                    i.fetch_add(1, Ordering::Relaxed),
                )
            },
            |(method, ud, i)| async move {
                assert_eq!(method.call::<usize>((ud, i)).await.unwrap(), 123 + i);
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
        table_create_empty,
        table_create_array,
        table_create_hash,
        table_get_set,
        table_traversal_pairs,
        table_traversal_for_each,
        table_traversal_sequence,
        table_ref_clone,

        function_create,
        function_call_sum,
        function_call_lua_sum,
        function_call_concat,
        function_call_lua_concat,
        function_async_call_sum,

        registry_value_create,
        registry_value_get,

        userdata_create,
        userdata_call_index,
        userdata_call_method,
        userdata_call_method_complex,
        userdata_async_call_method,
}

criterion_main!(benches);
