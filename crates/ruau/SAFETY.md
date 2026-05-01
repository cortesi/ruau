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

## Baseline (Stage Three)

| Metric                | `ruau` | `ruau-sys` |
| --------------------- | -----: | ---------: |
| `unsafe fn` (total)   |    175 |         81 |
| `pub unsafe fn`       |      1 |         77 |
| `unsafe { ... }` blocks |  239 |          0 |
| `unsafe impl`         |      8 |          0 |
| `unsafe extern`       |     31 |         30 |
| `SAFETY:` comments    |     53 |          0 |

The single remaining `pub unsafe fn` is `Luau::load_bytecode`. Its safety
contract is fundamental to the API: bytecode is not validated before
execution, so the caller must guarantee it came from a trusted Luau
compiler.

Stage Three introduced wrappers (`RawLuau::extra` / `extra_mut`,
`RawLuau::scoped_op`, `util::shim::{FfiResource, RawGuard}`) and converted
~49 raw `(*X.extra.get())` accesses to use the new accessors. The unsafe
counts moved slightly (+1 fn, +6 blocks) because each helper has its own
unsafe body, but `SAFETY:` density rose by 21 comments. Stage Four will
narrow the remaining whole-method unsafe blocks per module, which is where
the count reduction will land.

Notes:

- `unsafe extern` is the count of `unsafe extern "..." fn` declarations
  (FFI declarations and `extern "C-unwind"` callbacks). It is disjoint from
  `unsafe fn`, which counts plain `unsafe fn` definitions.
- `pub unsafe fn` is a strict count of true `pub` (externally visible)
  unsafe functions. `pub(crate)` and narrower visibilities are not included.

## Hotspots (Stage Three)

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
