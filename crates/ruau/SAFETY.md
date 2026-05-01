# `ruau` Safety Audit

This file is the audit anchor for the workspace's unsafe surface. It is updated
whenever the baseline numbers in `crates/ruau/audit-baseline.json` change. See
[`plans/unsafe.md`](../../plans/unsafe.md) for the staged reduction plan.

`ruau-sys` is a raw FFI binding to the Luau C API; its unsafe surface is
essentially fixed by the C headers and is **not** an audit target. The
auditable crate is `ruau`.

## Refreshing the numbers

```sh
cargo xtask unsafe-audit                # print current counts, soft-check
cargo xtask unsafe-audit --verbose      # also print per-file hotspots
cargo xtask unsafe-audit --update-baseline  # refresh audit-baseline.json
```

`cargo xtask tidy` runs the same audit at the end as a soft check (it never
fails the build at this stage; later stages will tighten this).

## Baseline (Stage Four — pass 2)

| Metric                | `ruau` | `ruau-sys` |
| --------------------- | -----: | ---------: |
| `unsafe fn` (total)   |    176 |         81 |
| `pub unsafe fn`       |      1 |         77 |
| `unsafe { ... }` blocks |  240 |          0 |
| `unsafe impl`         |      8 |          0 |
| `unsafe extern`       |     31 |         29 |
| `SAFETY:` comments    |    116 |          0 |

The single remaining `pub unsafe fn` is `Luau::load_bytecode`. Its safety
contract is fundamental to the API: bytecode is not validated before
execution, so the caller must guarantee it came from a trusted Luau
compiler.

Stage Four runs as a series of per-module passes.

- **Pass 1** swept `Registry::*` and `Luau::create_c_function` to use
  `scoped_op`, plus added SAFETY comments on `type_metatable`,
  `set_type_metatable`, `globals`, `current_thread`. Net: +12 SAFETY
  comments.
- **Pass 2** finished `state/mod.rs`: SAFETY comments on `sandbox`,
  the interrupt machinery (`set_interrupt`, `scoped_interrupt`,
  `remove_interrupt`, `interrupt_proc`), thread callbacks
  (`set_thread_callbacks`, `remove_thread_callbacks`,
  `userthread_proc`, `run_thread_collection_callback`), GC and memory
  paths (`gc_collect`, `gc_set_mode`, `used_memory`,
  `set_memory_limit`, `traceback`, `inspect_stack`), `Drop for Luau`,
  `Luau::new_with` / `inner_new`, the create_* family
  (`create_string`, `create_buffer*`, `create_table*`,
  `create_sequence_from`, `create_function`,
  `create_async_function`, `create_thread`, `create_userdata`,
  `create_opaque_userdata`, `create_proxy`, `register_userdata_type`),
  app_data accessors (`set_app_data`, `try_set_app_data`,
  `remove_app_data`), `yield_with`, `Luau::raw`, `WeakLuau::raw`,
  `WeakLuau::try_raw`, `LuauLiveGuard::deref`, `ScopedAppData::drop`,
  `ScopedInterrupt::drop`, the `compiler` and `enable_jit` setters,
  and `set_fflag`. Adds `# Safety` on `interrupt_proc`,
  `userthread_proc`, `Luau::raw_luau`, and
  `run_thread_collection_callback`. Net: +51 SAFETY comments.

Subsequent passes address `table.rs` (29 blocks), `state/raw.rs`
(24 blocks + 39 unsafe fns each needing `# Safety`), and the userdata
layer.

Notes:

- `unsafe extern` is the count of `unsafe extern "..." fn` declarations
  (FFI declarations and `extern "C-unwind"` callbacks). It is disjoint from
  `unsafe fn`, which counts plain `unsafe fn` definitions.
- `pub unsafe fn` is a strict count of true `pub` (externally visible)
  unsafe functions. `pub(crate)` and narrower visibilities are not included.

## Hotspots (Stage Four — pass 2)

Top-20 source files by combined unsafe weight (`unsafe fn` + `unsafe { }` +
`unsafe impl`). The rightmost column is `SAFETY:` comment density.

| File                                            |  fn | pubfn | block | impl | extern | SAFETY |
| ----------------------------------------------- | --: | ----: | ----: | ---: | -----: | -----: |
| `crates/ruau/src/state/mod.rs`                  |   4 |     1 |    65 |    0 |      3 |      9 |
| `crates/ruau/src/state/raw.rs`                  |  39 |     0 |    30 |    0 |      7 |     19 |
| `crates/ruau-sys/src/luau/compat.rs`            |  39 |    35 |     0 |    0 |      2 |      0 |
| `crates/ruau/src/conversion.rs`                 |  30 |     0 |     1 |    0 |      0 |      0 |
| `crates/ruau/src/table.rs`                      |   1 |     0 |    29 |    0 |      0 |      1 |
| `crates/ruau-sys/src/luau/lua.rs`               |  29 |    29 |     0 |    0 |     12 |      0 |
| `crates/ruau/src/analyzer.rs`                   |   4 |     0 |    16 |    5 |      0 |     18 |
| `crates/ruau/src/multi.rs`                      |  15 |     0 |     0 |    0 |      0 |      0 |
| `crates/ruau/src/util/mod.rs`                   |  14 |     0 |     1 |    0 |      3 |      0 |
| `crates/ruau/src/userdata_impl/mod.rs`          |   1 |     0 |    13 |    0 |      0 |      0 |
| `crates/ruau/src/userdata_impl/registry.rs`     |   1 |     0 |     8 |    3 |      0 |      0 |
| `crates/ruau-sys/src/luau/lauxlib.rs`           |  12 |    12 |     0 |    0 |      2 |      0 |
| `crates/ruau/src/thread.rs`                     |   3 |     0 |     9 |    0 |      0 |      0 |
| `crates/ruau/src/userdata_impl/ref.rs`          |   5 |     0 |     5 |    0 |      0 |      0 |
| `crates/ruau/src/state/extra.rs`                |   8 |     0 |     1 |    0 |      0 |      0 |
| `crates/ruau/src/util/userdata.rs`              |   9 |     0 |     0 |    0 |      0 |      0 |
| `crates/ruau/src/function.rs`                   |   0 |     0 |     8 |    0 |      2 |      0 |
| `crates/ruau/src/scope.rs`                      |   1 |     0 |     7 |    0 |      0 |      0 |
| `crates/ruau/src/traits.rs`                     |   6 |     0 |     1 |    0 |      0 |      1 |
| `crates/ruau/src/userdata_impl/cell.rs`         |   0 |     0 |     7 |    0 |      0 |      0 |

## Lint policy

Workspace-wide:

- `unsafe_op_in_unsafe_fn = "warn"`. Every unsafe operation inside an
  `unsafe fn` is individually surfaced. Stage Three converts this to
  `deny`.
- `clippy::undocumented_unsafe_blocks = "warn"`. Every `unsafe { ... }`
  block must have a `SAFETY:` comment. Stage Three converts to `deny`.
- `clippy::missing_safety_doc = "warn"`. Every `unsafe fn` must have a
  `# Safety` rustdoc section. Stage Five converts to `deny`.

Per-crate exceptions:

- `crates/ruau-sys/src/lib.rs` keeps `#![allow(unsafe_op_in_unsafe_fn)]`
  and `#![allow(clippy::missing_safety_doc)]`. Rationale: `ruau-sys` is
  raw FFI mirroring the Luau C ABI; per-symbol `# Safety` sections would
  add noise without improving the audit, and per-operation unsafe
  granularity is meaningless inside a thin binding.
- `crates/ruau/src/lib.rs` keeps `#![allow(unsafe_op_in_unsafe_fn)]`
  during Stage One only. Stage Three removes this allow and sweeps the
  resulting warnings.

## Audit policy

1. Treat unsafe as a property of a *block*, not of a *function*.
2. Every `unsafe { ... }`, `unsafe fn`, and `unsafe impl` site needs a
   `SAFETY:` comment naming the invariants relied on.
3. Cross-module helper unsafe functions are `pub(crate)`, not `pub`.
   The only externally visible unsafe function should be one whose
   safety contract is fundamental to the API.
4. New unsafe sites must not regress the audit baseline without an
   explicit `--update-baseline` commit. The `xtask unsafe-audit` soft
   check warns on regression today; Stage Three converts it to a hard
   gate.
