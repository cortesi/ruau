# ruau — Post-Luau-Specialization Simplification Plan

## Context

`ruau` is a fork of `mlua`, specialized from "generic Lua 5.1–5.4 + LuaJIT + Luau"
down to **Luau only**. The version-cfg purge is already done (no `lua54`/`lua53`
gates remain). What is left is *structural* mlua-era scaffolding: abstraction
layers that existed to span multiple runtimes and now wrap a single one.

This plan lists only changes that meet at least one bar:

- **(a)** meaningfully reduces lines of code, or
- **(b)** significantly reduces `unsafe`, or
- **(c)** removes a real layer of unnecessary abstraction.

Baseline metrics (for tracking): **30,564** src LOC, **169** `unsafe fn`,
`ruau-sys/src/luau/compat.rs` is **431** LOC.

Each finding below was verified against the source, not just inferred. A
"Considered and rejected" section at the end records plausible-looking changes
that do *not* meet the bar, with reasons — read it before re-litigating them.

---

## Top recommendations (by impact ÷ risk)

| # | Stage | Bar | Est. LOC | `unsafe` | Risk | Confidence |
|---|-------|-----|----------|----------|------|------------|
| 1 | Delete the `XRc` alias | (c)(a) | ~50 | — | Trivial | High |
| 2 | Trim `compat.rs` pure-rename / single-caller shims | (c)(a) | ~40 | small | Low | High |
| 3 | Consolidate error / protected-call helpers | (a)(b) | ~100 | −2 `unsafe fn` | Low–Med | High |
| 4 | Consolidate integer handling on Luau's native API | (c)(a) | ~80 | small | **Med** | High |
| 5 | Consolidate the userdata type registry | (c)(a) | ~100 | −3 blocks | **Med** | Med–High |
| 6 | Collapse the `GcMode` single-variant enum | (c)(a) | ~30 | — | Low | High |
| 7 | Flatten the `ruau-sys` build scripts | (c)(a) | ~25 | — | Trivial | High |
| 8 | Generic `BorrowedData<T>` for `BorrowedStr`/`Bytes` | (a)(c) | ~90 | — | Med | Med |
| 9 | Collapse the `HostApi` / `HostNamespace` builder split | (c)(a) | ~150 | — | Med | Med |

Total realistic reach: **~650 LOC** and **~2 `unsafe fn` + 3 `unsafe` blocks**
removed, but the more important payoff is the deletion of four genuine
abstraction layers (`XRc`, the integer-emulation compat shims, the build-script
indirection, the parallel host builders).

Recommended landing order is the table order: it is graduated from
mechanical/trivial to medium-risk, and earlier stages (1, 2) reduce the noise
that later stages have to edit through.

---

## Stage 1 — Delete the `XRc` alias

**Bar: (c) remove an abstraction layer, (a) reduce LOC. Risk: trivial.**

### Finding

`crates/ruau/src/types/sync.rs` is, in its entirety:

```rust
use std::rc::Rc;
pub type XRc<T> = Rc<T>;
```

In mlua, `XRc` switched between `Rc` and `Arc` under the `send` feature. ruau is
`!Send + !Sync` always (the VM is thread-pinned — see `lib.rs` module docs), the
`send` feature is gone, and there is no `XRefCell`/`MaybeSend`/`MaybeSync`
companion left. `XRc` is now a pure no-op alias: **47 references across 8 files**
(`state/{mod,raw,extra}.rs`, `types/{mod,value_ref}.rs`,
`userdata_impl/{cell,ref,registry}.rs`) plus `XRc::clone` / `XRc::new` /
`XRc::strong_count` / `XRc::into_inner` call forms. Every one is `Rc` spelled
with indirection that no longer means anything — a maintainer has to learn that
`XRc` is "just `Rc`".

### Steps

- [x] Mechanically replace `XRc` → `Rc` and `XRc::` → `Rc::` across the crate.
- [x] Delete `crates/ruau/src/types/sync.rs` and its `mod sync;` / re-export in
      `types/mod.rs`.
- [x] Update the three `types/mod.rs` aliases that name it
      (`InterruptCallback`, `ThreadCreationCallback`, `ThreadCollectionCallback`).
- [x] `cargo check` — the compiler finds every missed site.

### Impact

~50 LOC of churn removed, one file deleted, one fewer concept in the type
vocabulary. No behavior change.

---

## Stage 2 — Trim `compat.rs` pure-rename and single-caller shims

**Bar: (c) remove a translation layer, (a) reduce LOC. Risk: low.**

### Finding

`ruau-sys/src/luau/compat.rs` (431 LOC) is titled "Luau C API adapter helpers".
It re-creates the Lua 5.4 C-API *vocabulary* on top of Luau's native API, purely
because the high-level `ruau` crate was first written against Lua 5.4. On a
Luau-only fork, the shims that are **pure renames** are dead translation:

| Shim (`compat.rs`) | Native Luau equivalent | ruau callers | Action |
|---|---|---|---|
| `lua_rawlen` (89) | `lua_objlen` | 4 | delete, call native |
| `lua_rawgetp` (123) | `lua_rawgetptagged(L,i,p,0)` | 7 | delete, call native |
| `lua_rawsetp` (157) | `lua_rawsetptagged(L,i,p,0)` | 6 | delete, call native |
| `lua_copy` (55) | (genuine, 1 caller) | 1 | inline at the one call site |
| `luaL_loadbuffer` (334) | wraps `luaL_loadbufferenv(…,0)` | 2 | inline the env=0 default |
| `luaL_checkinteger` (243) | — | 0 (private) | folds away with Stage 4 |

`lua_rotate`, `lua_geti`/`lua_seti`, `lua_getuservalue`/`lua_setuservalue`,
`luaL_loadbufferenv`, `lua_resumex`, `luaL_len`, `luaL_checkstack`,
`luaL_tolstring` are **genuine reimplementations** of behavior Luau's raw API
does not provide — keep them.

`lua_rawgeti` (11 callers) and `lua_rawseti` (13 callers) are thin
`try_into().expect()` wrappers over native `lua_rawgeti_`/`lua_rawseti_`. They
are *defensible* but optional: most callers pass small literal indices that
could pass `c_int` directly. Treat these as a stretch goal, not a requirement.

### Steps

- [x] Delete `lua_rawlen`, `lua_rawgetp`, `lua_rawsetp` from `compat.rs`; update
      the `pub use compat::{…}` list in `luau/mod.rs`; migrate the ~17 call sites
      in `ruau` to the native names.
- [x] Inline `lua_copy` into its single caller (`util/mod.rs`) and delete the shim.
- [x] Inline `luaL_loadbuffer`'s env=0 default into its 2 callers, or keep it —
      it is one line; decide during execution.
- [x] Stretch: evaluate dropping `lua_rawgeti`/`lua_rawseti` in favour of the
      native `_`-suffixed forms with explicit `c_int` indices.
- [x] Run `cargo xtask test` (the FFI boundary is exercised broadly by the suite).

Execution note: kept `lua_rawgeti` / `lua_rawseti` for now. Unlike the removed
pointer and length shims, these preserve the crate's `lua_Integer` indexing
vocabulary at call sites and centralize the fallible `c_int` narrowing in one
place; deleting them would spread conversion noise without reducing a real
layer.

### Impact

~40 LOC out of `compat.rs`, plus the conceptual win: `compat.rs` shrinks toward
*only* genuine Luau-gap reimplementations, so the file's title stops lying.

---

## Stage 3 — Consolidate the error / protected-call helpers

**Bar: (a) reduce LOC, (b) remove `unsafe fn`. Risk: low–medium.**

### Finding

The Rust↔C error-smuggling path carries two pairs of helpers where mlua needed
the spread for cross-version reasons; Luau's single, consistent C++-exception
model means each pair can collapse to one:

1. **`callback_error` vs `callback_error_ext`.**
   `util/error.rs:54` `callback_error` has exactly **2 callers**, both inside
   `util/error.rs` itself (`error_tostring` at :300, `destructed_error` at :349).
   `state/util.rs:92` `callback_error_ext` is the real workhorse used everywhere
   else (`state/{mod,raw}.rs`, `runtime/globals.rs`, `userdata_impl/util.rs`),
   and it already accepts a null `ExtraData` pointer
   (`callback_error_ext(state, ptr::null_mut(), false, …)` — see
   `state/mod.rs:730`). The two `callback_error` callers can route through
   `callback_error_ext` with a null extra, letting the whole `callback_error`
   `unsafe fn` (~100 LOC incl. its inline stack management) be deleted.

2. **`protect_lua_call` vs `protect_lua_closure`.**
   The `protect_lua!` macro (`macros.rs:81`) has two arms: a closure arm →
   `protect_lua_closure`, and a `fn($state) $code` arm that synthesizes a
   `do_call` C function → `protect_lua_call`. The second arm can expand to a
   closure (`|$state| { $code; … }`) and also use `protect_lua_closure`,
   letting `protect_lua_call` (`util/error.rs:161`, an `unsafe fn`) be deleted.
   The compiler inlines the closure; there is no perf delta.

### Steps

- [x] Rewrite `error_tostring` and `destructed_error` to call
      `callback_error_ext(state, ptr::null_mut(), true, …)`; delete
      `callback_error`.
- [x] Rewrite the `protect_lua!` `fn(...)` macro arm to expand to a closure
      dispatched through `protect_lua_closure`; delete `protect_lua_call`;
      drop its re-export in `util/mod.rs`.
- [x] Confirm `tests/error.rs` and the panic/traceback tests still pass.

New finding during execution: the original `callback_error` always wrapped Rust
errors raised by `error_tostring` / `destructed_error` as `CallbackError`.
Using `callback_error_ext(..., false, ...)` changed the already-resumed panic
case from `CallbackError { cause: PreviouslyResumedPanic }` to a direct
`PreviouslyResumedPanic`. The implementation therefore uses `wrap_error = true`
for these two callbacks to preserve the old public error shape.

### Impact

~100 LOC and **2 `unsafe fn`** removed. One catch-and-wrap path instead of two.
Note: keep `WrappedFailure` and its pool, and keep the native-traceback path
(commit `74539b7`) — those are load-bearing, not scaffolding.

Post-stage measurement: **30,444** Rust src LOC and unsafe-audit reports
`ruau` at **93** `unsafe fn` (down from 95 before this stage) and `ruau-sys` at
**66** `unsafe fn`.

---

## Stage 4 — Consolidate integer handling on Luau's native API

**Bar: (c) remove the integer-emulation layer, (a) reduce LOC. Risk: medium —
this one changes an observable Luau-side type, so it needs test verification.**

### Finding

This is the highest-value structural finding, and it also resolves a latent
inconsistency.

The vendored Luau has a **native integer type**: `LUA_TINTEGER = 4` (distinct
from `LUA_TNUMBER = 3`), plus native `lua_pushinteger64(i64)`,
`lua_tointeger64(…)`, `lua_isinteger64(…)` (see `ruau-sys/src/luau/lua.rs`).

But `compat.rs` still carries the *old emulation* shims from when Luau had no
integers:

- `compat::lua_pushinteger` (`compat.rs:63`) → `lua_pushnumber(i as f64)` — i.e.
  it pushes a **`number`**, discarding the integer type tag.
- `compat::lua_tointegerx` (`compat.rs:72`) → reads via `lua_tonumberx` and
  checks for an integral double.

The `ruau` crate is **half-migrated** and inconsistent:

- **Write path** (`state/raw.rs:766`) pushes `Value::Integer(i)` via
  `ffi::lua_pushinteger` — the stale shim — so a Rust integer lands on the Luau
  stack typed as `number`, not `integer`.
- **Read path** (`state/raw.rs:822`) *does* handle native `LUA_TINTEGER` via
  `lua_tointeger64` → `Value::Integer`. And the `LUA_TNUMBER` arm
  (`state/raw.rs:816-819`) additionally runs a "is this double secretly an
  integer" recovery dance — legacy compensation for the broken write path.
- `conversion.rs` mixes both: `lua_tointegerx` (compat, :876) *and*
  `lua_tointeger64` (native, :888) in adjacent code.

So `Value::Integer` is legitimate (Luau genuinely has integers — do **not**
delete the variant), but the emulation shim layer underneath it is both
redundant *and* the cause of a write/read asymmetry.

### Steps

- [ ] Point the write path at native `lua_pushinteger64`: update
      `state/raw.rs:766`, `conversion.rs:837`, and the async-results fast path
      (`state/raw.rs:140,148`).
- [ ] Point the read path uniformly at `lua_tointeger64` / `LUA_TINTEGER`;
      delete the integer-recovery branch in the `LUA_TNUMBER` arm
      (`state/raw.rs:816-819`) — with a true integer type tag it is dead.
- [ ] Delete `compat::{lua_pushinteger, lua_tointeger, lua_tointegerx,
      luaL_optinteger, luaL_checkinteger}` and their `luau/mod.rs` re-exports.
- [ ] **Verification gate:** add/extend tests asserting the Luau-side `type()` of
      a Rust-pushed integer, and round-trip `Value::Integer`. Confirm
      `tests/{conversion,value,serde}.rs` still pass. This is the step that makes
      the medium risk acceptable — do not skip it.

### Impact

~80 LOC removed (5 compat shims + the recovery branch + the mixed conversion
paths collapse), the integer-emulation layer is gone, and `Value::Integer`
becomes consistently a real Luau integer in both directions.

---

## Stage 5 — Consolidate the userdata type registry

**Bar: (c) remove redundant indirection, (a) reduce LOC. Risk: medium.**

### Finding

`ExtraData` (`state/extra.rs:44-50`) holds **five `FxHashMap`s plus a manual
one-entry cache** for one concern — "which Rust type is this userdata":

```rust
pending_userdata_reg:            FxHashMap<TypeId, RawUserDataRegistry>,
registered_userdata:             FxHashMap<TypeId, RegisteredUserData>,   // {mt_ref, tag}
registered_userdata_tag_types:   FxHashMap<c_int, TypeId>,                // inverse of .tag
registered_userdata_mt:          FxHashMap<*const c_void, Option<TypeId>>,
registered_userdata_serializers: FxHashMap<TypeId, UserDataSerializeCallback>,
last_checked_userdata_mt:        (*const c_void, Option<TypeId>),         // hand-rolled cache
```

This is mlua-era breadth. On Luau, userdata carries a native integer **tag**
(`lua_newuserdatatagged` / `lua_userdatatag`), and tags here are allocated
**densely and sequentially** from 2 (`next_userdata_tag`, `extra.rs:50,152`;
`allocate_userdata_tag`, `raw.rs:1086-1090`). That makes most of this redundant:

- `registered_userdata_tag_types` (tag → `TypeId`) is the inverse of the `.tag`
  field already in `registered_userdata`. Because tags are dense from 2, a
  `Vec<TypeId>` indexed by `tag - 2` replaces the whole `FxHashMap` with O(1)
  array access and no hashing — and it is the *hot* path (`raw.rs:1355`, hit on
  every typed `borrow()`).
- `registered_userdata_serializers` (`TypeId` → callback) is a sparse side-table
  that belongs as an `Option<…>` field on `RegisteredUserData`.
- `registered_userdata_mt` + `last_checked_userdata_mt` are the metatable-pointer
  fallback path. Keep the fallback, but it should be the *only* mt map, not one
  of three overlapping ones.

### Steps

- [ ] Add `serializer: Option<UserDataSerializeCallback>` to `RegisteredUserData`;
      delete `registered_userdata_serializers`; update `raw.rs:1061-1065,1289,1308`.
- [ ] Replace `registered_userdata_tag_types` with a `Vec<TypeId>` indexed by
      `tag - FIRST_TAG`; update the registration site (`raw.rs:1070`) and the
      lookup (`raw.rs:1355`).
- [ ] Re-check whether `pending_userdata_reg` still needs to be separate from
      `registered_userdata` once the above lands, or whether the "pending" state
      can be a variant/flag.
- [ ] Run `cargo xtask test` with focus on `tests/userdata.rs`,
      `tests/serde.rs`, `tests/scope.rs`.

### Impact

~100 LOC and **2 `FxHashMap`s** removed from `ExtraData`; the per-`borrow()`
type check becomes an array index instead of a hash lookup; ~3 `unsafe`
`extra_mut()` regions in the lookup helpers simplify or disappear.

---

## Stage 6 — Collapse the `GcMode` single-variant enum

**Bar: (c) remove a vestigial abstraction, (a) reduce LOC. Risk: low.**

### Finding

`state/mod.rs:137`:

```rust
#[non_exhaustive]
pub enum GcMode {
    Incremental(GcIncParams),   // the only variant
}
```

Luau has exactly one GC: incremental mark-and-sweep (no generational mode — that
was Lua 5.4). `GcMode` is a one-variant enum modelling a choice that does not
exist; `gc_set_mode(GcMode)` (`state/mod.rs:959`) immediately matches the single
arm. The real configuration surface is `GcIncParams` (`goal`,
`step_multiplier`, `step_size` — all `Option<c_int>`), which is genuine and
should stay.

### Steps

- [ ] Replace `gc_set_mode(GcMode)` with a method that takes `GcIncParams`
      directly (e.g. `gc_set_params` / `gc_configure`).
- [ ] Delete the `GcMode` enum and its `lib.rs` re-export.
- [ ] Update the one in-tree user, `tests/memory.rs:80`.

### Impact

~30 LOC and one public type removed; the GC-tuning API stops implying a mode
choice that Luau does not offer.

---

## Stage 7 — Flatten the `ruau-sys` build scripts

**Bar: (c) remove indirection, (a) reduce LOC. Risk: trivial.**

### Finding

The build is split across three files for no current reason:

- `build/main.rs` — one line: `include!("main_inner.rs")`.
- `build/main_inner.rs` — `mod find_vendored;` + a 5-line `main()`.
- `build/find_vendored.rs` — `probe_lua()`, the actual ~35-line script.

The `main.rs`/`main_inner.rs` split and the `find_vendored` name are mlua-sys
heritage, where the build chose between *system* and *vendored* Lua across
*multiple versions*. ruau is vendored-Luau-only; there is nothing to "find" and
nothing to switch on. Separately, `ruau-luau-src/src/lib.rs` carries a
`use_longjmp` config field + `use_longjmp()` setter (`lib.rs:26,50,93,282`) with
**no callers** anywhere in the workspace.

### Steps

- [ ] Merge the three build files into a single `build/main.rs` (drop the
      `include!` and the `find_vendored` indirection; `probe_lua`'s body becomes
      `main`'s body).
- [ ] Delete the unused `use_longjmp` field, setter, and its `base_config`
      branch from `ruau-luau-src`.
- [ ] `cargo clean -p ruau-sys && cargo build -p ruau-sys` to confirm the build
      script still links Luau + the analyze shim.

### Impact

~25 LOC, two files, and one dead config knob removed; the build is one
top-to-bottom script.

---

## Stage 8 — Generic `BorrowedData<T>` for `BorrowedStr` / `BorrowedBytes`

**Bar: (a) reduce LOC, (c) collapse duplicated structure. Risk: medium.**

### Finding

`string.rs` defines `BorrowedStr` (`:258`) and `BorrowedBytes` (`:347`) as two
structs with the **same three fields** (`&'static` slice, `ValueRef`,
`LuauLiveGuard`) and **near-identical trait impl blocks** — `Deref`, `Borrow`,
`AsRef`, `Debug`, `PartialEq<T>`, `Eq`, `PartialOrd<T>`, `Ord` are written out
twice, differing only in `str` vs `[u8]` (~85 lines each, ~10 impl blocks).

A single `BorrowedData<T: ?Sized>` generic over the target type, with
`pub type BorrowedStr = BorrowedData<str>;` /
`pub type BorrowedBytes = BorrowedData<[u8]>;`, writes those impls once.

### Steps

- [ ] Introduce `BorrowedData<T: ?Sized>` holding the slice/ref/guard generically.
- [ ] Move the shared impls (`Deref`, `Borrow`, `AsRef`, `Debug`, `PartialEq`,
      `Eq`, `PartialOrd`, `Ord`) to generic impls.
- [ ] Keep the type-specific bits as the only non-generic code: `Display` (str
      only), `IntoIterator` (bytes only), and the `TryFrom`/`From<&LuauString>`
      constructors.
- [ ] Add `BorrowedStr` / `BorrowedBytes` aliases; confirm the public names and
      `LuauString::{to_str,as_bytes}` return types are unchanged.

### Impact

~90 LOC of duplicated trait impls removed; the two types stay
source-compatible as aliases.

---

## Stage 9 — Collapse the `HostApi` / `HostNamespace` builder split

**Bar: (c) remove a duplicated abstraction, (a) reduce LOC. Risk: medium —
public API change.**

### Finding

`host.rs` (553 LOC) carries two parallel builder surfaces with mirrored method
families:

- `HostApi` — `global_function`, `global_async_function`, plus
  `namespace`/`try_namespace`.
- `HostNamespace` (`host.rs:286-490`, ~200 LOC) — `function`, `try_function`,
  `async_function`, `try_async_function`, *its own* nested
  `namespace`/`try_namespace`, plus `write_table_type` / `build_table`.

`HostNamespace` only ever exists transiently inside a `HostApi::namespace(name,
|ns| …)` callback — it is never a standalone value. The `function` family is the
`global_function` family with a path prefix. This is one builder modelled as
two, with the function-registration logic written twice.

### Steps

- [ ] Decide the unified shape: either a path-style API
      (`HostApi::function("math/add", …)`) or a single re-entrant builder that
      `namespace` hands back a scoped view of (sharing one impl).
- [ ] Fold `write_table_type` / `build_table` and the signature/identifier
      validation (`check_function_signature`, `is_luau_identifier`, …) into the
      single path; they are namespace-shaped today but type-agnostic.
- [ ] Update `examples/` and `tests/` that build namespaced host APIs, and the
      `lib.rs` re-exports.
- [ ] This stage is the least pre-designed here — confirm the target API with a
      quick sketch before committing to the refactor.

### Impact

~150 LOC of mirrored builder methods removed; one host-registration code path
instead of two. Touch last: it is the only stage with a non-trivial public API
change, so it benefits from the noise-reduction of Stages 1–8 first.

---

## Considered and rejected

These came up during review (several were proposed by the exploration sweep) but
do **not** meet the bar — recorded so they are not re-investigated:

- **Delete `Value::Integer`.** Rejected: Luau has a *native* integer type
  (`LUA_TINTEGER`), so the variant is correct. The real issue is the emulation
  *shims* underneath it — that is Stage 4.
- **Replace the userdata `RwLock` (`userdata_impl/lock.rs`) with `std::RefCell`.**
  Rejected: the custom 134-line cell *returns* `Error::UserDataBorrowError` on
  conflict; `RefCell` *panics*. Panicking across the Luau FFI boundary on a
  borrow conflict is wrong behavior. The file is already minimal — keep it.
- **Replace the `ref_thread` with `luaL_ref(LUA_REGISTRYINDEX)`.** Rejected: the
  auxiliary-thread ref stack is a deliberate *performance* mechanism (stack
  push/pop vs. registry hashing) and isolates ruau handles from user-visible
  registry state. It is load-bearing, not scaffolding, and the LOC payoff is
  small relative to the risk.
- **Merge `Luau` and `RawLuau` into one type.** Rejected for now: the
  public-handle / callback-borrow split is real architecture; merging is a
  high-risk rewrite with speculative payoff. Revisit only if a concrete pain
  point emerges.
- **Replace `StdLib` bitflags with a bool.** Rejected: the granular flags
  (`StdLib::MATH | StdLib::STRING | …`) are a *used* feature — callers load
  library subsets (`tests/`, `stdlib.rs` docs). A bool removes capability; that
  is feature removal, not simplification.
- **Delete `ErrorContext`, `LuauInterruptPolicy`, `SafetyError`.** Rejected:
  `ErrorContext` and `LuauInterruptPolicy` are public, documented, and exercised
  by `tests/`. `SafetyError` is referenced by `worker/mod.rs`. Removing live,
  tested API is not simplification.
- **Replace `short_type_name` with `std::any::type_name`.** Rejected: the helper
  deliberately *shortens* type names for user-facing error messages;
  `type_name` emits fully-qualified paths. Swapping it degrades error output.
- **`ChunkMode` enum → `bool`, `MultiValue` `VecDeque` → `Vec`, `value_visit`
  collapse.** Rejected as not meeting the bar: a 2-variant enum is not an
  "unnecessary layer" (and `is_binary: bool` reads worse than `ChunkMode::Binary`);
  the `MultiValue` container choice is a ~30-LOC internal detail, not structural;
  `value_visit` has multiple real callers and a trait boundary that earns its keep.

## General guidance for execution

- Land stages **in table order**. 1–2 and 6–7 are mechanical and can go in quick
  successive commits; 3 next; 4, 5, 8, 9 each deserve their own commit + test run.
- After each stage: `cargo xtask test` (the project's comprehensive sweep), and
  re-run `cargo xtask` unsafe-audit checks for Stages 3–5.
- Stage 4 has a **hard verification gate** — do not land it without the
  Luau-side `type()` assertions; it is the one stage that changes observable
  runtime behavior.
- Re-measure baseline metrics (src LOC, `unsafe fn` count) after Stages 3, 5, and
  9 to confirm the plan is tracking.
- One commit per stage, each self-contained and green, so any stage can be
  reverted independently if needed.
