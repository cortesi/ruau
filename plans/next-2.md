# ruau: Strategic Direction and Analyzer Integration

This document picks up where `plans/luau.md` and `plans/drop-reentrant-mutex.md`
leave off. It covers (1) the architectural simplifications that should land
after the reentrant-mutex removal, (2) modern-Rust upgrades that mlua cannot
easily take, and (3) how to bring the `luau-analyze` feature set into this
workspace — including whether we extend `luau0-src` or fork it.

## What's already done

- Forked from mlua, structurally intact (state/, types/, userdata/, util/,
  luau/, serde/).
- Three published crates: `ruau-sys`, `ruau`, `ruau_derive`, plus an `xtask`
  workspace member.
- `ruau-sys` statically links Luau via `luau0-src 0.20.0` as a build
  dependency, with `links = "luau"`. No dlopen, no symbol-hiding.
- Non-Luau dialects fully removed (commits `264ba0b`, `c1b283f`, `1a46da4` ≈
  2.9k LoC deleted across cfg gates and dead test branches).
- Tending infrastructure: workspace lints, `xtask`, formatting, codecov.
- Existing plans:
  - `plans/luau.md`: small rename/lint polish backlog.
  - `plans/drop-reentrant-mutex.md`: removes `parking_lot::ReentrantMutex` to
    make `Lua: Send + !Sync`. Not yet executed.

The fork is at the "compiles cleanly as Luau-only mlua" point. Everything below
assumes `drop-reentrant-mutex.md` lands first; some items below depend on it.

## Strategic shape

The end state is a Luau-specific embedding library that mlua cannot easily
become because of mlua's need to support every Lua dialect:

1. **Single canonical Luau build.** No vector-size toggle, JIT always linked,
   strict-only solver as the runtime contract.
2. **Tokio-only async.** No runtime-agnostic plumbing.
3. **Sync data access, async-only execution.** Reading values stays sync;
   `call`/`exec`/`eval` for Luau code returns a future, always.
4. **`Lua: Send + !Sync`.** No reentrant lock. Cross-thread orchestration
   via channels, single-task-affinity for the Lua handle.
5. **Tagged userdata as the universal mechanism.** Per-type tags, not the
   tag=1-wrapper-around-`Box<dyn Any>` pattern inherited from mlua.
6. **Atom-based `__namecall`.** Method names atomized at load; dispatch by
   integer atom, not string compare.
7. **Analyzer first-class.** A sibling `ruau-analyze` crate built from the
   same Luau source pin, with shared schema/require/types between checker
   and runtime.

Items 1-6 are simplifications; item 7 is the genuine differentiation.

## Stage 0: Land the existing plan first

`plans/drop-reentrant-mutex.md` is a prerequisite. Items 4-6 below assume
`Lua` is `Send + !Sync` and the `lock()`/`with_lock` ceremony is gone. Do not
start the work below until that plan completes through Stage Five validation.

## Stage 1: Build configuration cleanup

Goal: collapse build-time variance to one configuration.

1. [ ] Remove `luau-vector4`. Vector is 3-wide. Keep the Rust-side `Vector`
   type 3-wide as well; if a 4-wide need ever appears, add it as a separate
   type, not a feature gate.
2. [ ] Make `luau-jit` non-optional: always compile and link CodeGen, expose
   a runtime `Lua::enable_jit(bool)` that defaults to `false`. Build-time
   cost is ~30% on the C++ side; worth it for one-build simplicity.
3. [ ] Audit `Cargo.toml` features. Target end state:
   - keep: `async`, `serde`, `macros`, `anyhow`
   - remove: `luau-jit`, `luau-vector4`, `send`, `error-send`,
     `userdata-wrappers`, `serialize` (deprecated alias)
4. [ ] Remove `parking_lot` from dependencies once `userdata-wrappers` is
   gone. Confirm nothing else needs it.

## Stage 2: Async commitment

Goal: tokio-only, async-by-default for Luau execution, sync Rust callbacks
remain available for cheap cases.

1. [ ] Decide tokio commitment. Replace `futures-util` with direct
   `tokio::sync::Notify` for waker signalling and `tokio::task::AbortHandle`
   for cancellation. Drop async-runtime-agnostic plumbing.
2. [ ] Drop sync `Function::call`, `Chunk::exec`, `Chunk::eval`, `Chunk::call`.
   The async variants become the only execution paths and lose their `_async`
   suffix — `Function::call(...).await` is the only call shape.
3. [ ] Keep both `create_function` (sync `Fn`) and `create_async_function`
   (async). Sync Rust callbacks stay because they avoid the coroutine wrapper
   for trivial helpers.
4. [ ] First-class cancellation: `AsyncThread<R>: Drop` triggers a coroutine
   interrupt that surfaces as `Error::Cancelled`. Wire it through
   `lua_callbacks(...).interrupt`.
5. [ ] Built-in `tokio::time::sleep` and `tokio::task::yield_now` exposed as
   Lua functions in `configure_luau`, opt-in via a `Lua::install_tokio_helpers`
   call.

## Stage 3: Modern Rust idioms

Goal: shrink the public surface using language features mlua predates.

1. [ ] Switch `create_async_function` to `impl AsyncFn(Lua, A) -> Result<R>`
   (async closures are stable in 1.85; ruau's MSRV is 1.88, so this is free).
   Remove the boxing dance in the closure type alias.
2. [ ] Make `Require` and any other user-implementable async trait return
   `impl Future` directly (RPITIT). Remove `BoxFuture` allocations on the
   require hot path.
3. [ ] Replace hand-rolled `Display`/`Error` impls in `error.rs` with
   `thiserror`. Cosmetic but removes ~150 LoC.
4. [ ] Add `tracing` instrumentation on every `protect_lua_*` call, every
   chunk load, and every async resume. Zero-cost when no subscriber is
   installed; free observability for users.
5. [ ] Audit `Box<dyn Future>` vs `Pin<Box<dyn Future>>` boundaries. With
   async closures and RPITIT, several public signatures simplify.

## Stage 4: Tagged userdata as the universal model

Goal: replace the inherited `tag = 1, store Box<dyn Any> inside` pattern with
per-type tags. This is the largest refactor below the analyzer work.

1. [ ] Per-`Lua` tag registry: `(TypeId → c_int)` map in `ExtraData`. Each
   call to `register_userdata_type::<T>()` allocates a fresh tag.
2. [ ] All `T: UserData` instances are pushed via
   `lua_newuserdatataggedwithmetatable(state, sizeof(T), tag)`. Metatables
   are bound per-tag via `lua_setuserdatametatable(state, tag)`, so
   metatable lookup is zero-cost.
3. [ ] `lua_setuserdatadtor(state, tag, dtor)` registers Rust drop globally
   per tag, not via the `__gc` metamethod. Removes one Lua call per drop.
4. [ ] `FromLua for T` becomes `lua_userdatatag(state, idx) == registered_tag`
   plus a pointer cast. No more "fetch metatable, compare type-id key,
   downcast through Box<dyn Any>". Faster and the type-erased path is gone.
5. [ ] Scope semantics: `scope.create_userdata(...)` registers the value with
   a tag that gets its metatable swapped to a "destructed" sentinel on scope
   exit. Mostly removes the bespoke scope-userdata machinery in `userdata/`.
6. [ ] Static assertions that registered tags survive `Lua` move (they do,
   tags are state-local) and are unique per type per state.

This is the single largest LoC delta, but cleans up the userdata directory
substantially. Estimate: 2-3 weeks, ~1500 LoC net deletion after the
refactor.

## Stage 5: Atom-based namecall

Goal: zero-allocation method dispatch on the hot path.

1. [ ] Register an atom callback at state creation via Luau's
   `lua_callbacks(state)->useratom` (single function pointer per state).
   Method names get atomized at parse/load time.
2. [ ] Per-`Lua` map `name -> atom` for atoms we've assigned, plus the
   reverse for diagnostics. Names not in the map fall back to string compare.
3. [ ] In `__namecall`, fetch the atom via `lua_namecallatom(state, &atom)`,
   index a per-userdata-tag method table by atom directly. Today
   `userdata/util.rs:440-465` does string lookup in a `FxHashMap<Vec<u8>>`;
   replace that with a `Vec<Option<CallbackPtr>>` indexed by atom.
4. [ ] Benchmark: `script-bench-rs` namecall-heavy benches should show
   measurable improvement. If they don't, revert.

## Stage 6: `luau-analyze` integration

This is the main strategic decision and the biggest single value-add. The
constraint is that Luau's `Analysis` component is **not** in `luau0-src`.
That crate ships only `Ast`, `CodeGen`, `Common`, `Compiler`, `Config`,
`Custom`, `Require`, `VM`. Analysis is 3.2 MB / 73 .cpp / 91 .h files —
too big to be folded in by accident.

### The sourcing question

Three options, in order of preference:

**Option A (preferred): fork `luau0-src` as `ruau-luau-src`.**

Rename and republish under our control. Add `Analysis` as an opt-in feature.
This becomes the single source of Luau truth for the workspace.

- Pros: one Luau pin for runtime + analyzer; full control over which
  components compile; can add things `luau0-src` doesn't expose
  (TimeTrace, FileUtils, AnalyzeRequirer.cpp from the CLI tree).
- Cons: we now own Luau vendoring and have to bump it ourselves. In practice
  the bump cadence is the same as the existing `luau-analyze` playbook —
  monthly-ish, mechanical.

Concretely: copy `luau0-src` source layout, rename to `ruau-luau-src`, add
`analysis = []` cargo feature that includes the `Analysis/` sources and the
small set of CLI files (`AnalyzeRequirer.cpp`, `FileUtils.cpp`,
`VfsNavigator.cpp`) that `luau-analyze` already compiles. Expose a richer
`Build` API: `enable_analysis(bool)`, `enable_codegen(bool)`. Keep
`enable_codegen` default-on for ruau-sys consumers (matches Stage 1).

**Option B: upstream a PR to `luau0-src` adding optional Analysis.**

Same maintainer as mlua (zxteam). Worth trying — clean engineering, low
maintenance for them, modest upside for them. But we should not block on
acceptance; if it happens, great, otherwise fall back to A. I'd file the PR
as a courtesy in parallel with starting Option A.

**Option C: vendor only Analysis in ruau-sys, alongside `luau0-src`.**

Keep `luau0-src` for the runtime pieces; add an `Analysis/` submodule in
`ruau-sys` and compile it with our own `cc::Build`. Reject this. The version
alignment burden between two independent Luau pins (luau0-src's and ours)
is exactly the integration risk we're trying to eliminate.

### Recommended layout

```
ruau-workspace/
├── crates/
│   ├── ruau-luau-src/        # fork of luau0-src + Analysis support
│   ├── ruau-sys/             # FFI bindings (existing)
│   ├── ruau/                 # high-level runtime (existing)
│   ├── ruau-analyze/         # NEW: Frontend, Checker, schema extraction
│   ├── ruau_derive/          # existing
│   └── xtask/                # existing
└── ...
```

`ruau-sys/Cargo.toml`: replace `luau0-src` build-dep with
`ruau-luau-src = { version = "...", features = [] }`. No analysis here.

`ruau-analyze/Cargo.toml`: depends on `ruau-luau-src` with
`features = ["analysis"]` plus on `ruau-sys` for the shared lua_State type
definitions and any FFI shapes the analyzer reuses.

### What to bring in from `luau-analyze`

The current `~/git/public/luau-analyze` is ~2.7k LoC across:
- `shim/analyze_shim.cpp` (877 lines, mature, recently de-slopped)
- `src/lib.rs` (1023 lines: `Checker`, `Diagnostic`, `CancellationToken`,
  `VirtualModule`, `EntrypointSchema`, `extract_entrypoint_schema`)
- `src/module_schema.rs` (479 lines, pure Rust, hand-rolled `.d.luau`
  scanner — `ModuleSchema`, `ClassSchema`, `NamespaceSchema`,
  `extract_module_schema`)
- `src/ffi.rs` (369 lines, libloading-based — discard, regenerate as
  plain `extern "C"` blocks)

Migration plan:

1. [ ] Stand up `ruau-luau-src` (Option A above). One commit.
2. [ ] Stand up `ruau-analyze` skeleton: empty crate, Cargo deps, `links`
   metadata. One commit.
3. [ ] Copy `analyze_shim.cpp` into `ruau-analyze/shim/`. Build it via
   `cc::Build` against headers exposed by `ruau-luau-src` with the
   `analysis` feature on. Drop the symbol-hiding flags (no dlopen, we're
   statically linking).
4. [ ] Replace `src/ffi.rs`'s libloading machinery with `extern "C"` blocks
   in `ruau-analyze/src/ffi.rs`. ~300 LoC savings.
5. [ ] Port `src/lib.rs` to `ruau-analyze/src/lib.rs`. Make it depend on
   `ruau` types where it makes sense (e.g., `ruau::Error` as the
   conversion target for analyzer failures — see "Shared types" below).
6. [ ] Port `src/module_schema.rs` essentially as-is. It's pure Rust with
   no Luau dependency; no changes needed.
7. [ ] Port `tests/`. The luau-analyze test fixtures should slot in unchanged.

### Shared types between analyzer and runtime

This is where the integration earns its keep. Concretely:

1. [ ] **Shared `Require` trait.** Currently the runtime's
   `ruau::luau::Require` and the analyzer's `FileResolver` are distinct.
   Define one trait in `ruau-luau-src` (or in a shared `ruau-core` crate
   if neither is appropriate); both ends accept it. A test fixture that
   declares virtual modules works identically at check-time and run-time.
2. [ ] **`ruau-analyze::ModuleSchema → ruau::UserDataRegistry`.** Add a
   helper `ruau::UserDataRegistry::from_class_schema(schema: &ClassSchema)`
   that registers a userdata tag with method stubs matching the declared
   methods. This is the schema-driven registration that mlua structurally
   cannot offer.
3. [ ] **`EntrypointSchema → Function::call`.** When the analyzer extracts
   parameter types for a `return function(...)` chunk, expose those at
   load time so callers can type-check arguments (or at least produce
   readable errors) before invocation.
4. [ ] **Unified `CancellationToken`.** The analyzer's
   `FrontendCancellationToken` and the runtime's interrupt callback both
   already exist. One shared `CancellationToken` type from the runtime
   that the analyzer's check loop also respects. Drop happens once.

### What stays in luau-analyze (the existing public crate)

The existing `luau-analyze` crate at `~/git/public/luau-analyze` is the
library tag1k currently uses. Don't break it during this work. Two paths:

- **Path A: freeze it at 0.x, point new users at `ruau-analyze`.** Cleanest
  if we don't have outside users yet (we don't, the crate is at 0.0.1).
- **Path B: re-implement `luau-analyze`'s public API on top of
  `ruau-analyze`.** A thin shim crate, kept for whoever's already
  depending on it. Probably overkill given the version.

I'd take Path A: bump `luau-analyze` to 0.0.2 with a deprecation notice
in the README pointing at `ruau-analyze`, and stop adding features there.

## Stage 7: Polish and publish

1. [ ] Documentation pass: every public type gets a worked example.
   `examples/` covers analyzer + runtime integration end-to-end.
2. [ ] Benchmark suite: extend `script-bench-rs` comparisons with the
   atom-namecall and tagged-userdata wins.
3. [ ] Decide naming. The `mlua` → `ruau` rename has happened in the crate
   names; the *type* names (`Lua`, `Function`, `Table`, etc.) are still
   neutral. Don't rename them — they read naturally.
4. [ ] Publish `ruau-luau-src`, `ruau-sys`, `ruau`, `ruau-analyze`,
   `ruau_derive` together. Coordinate version numbers via the workspace
   `package.version` mechanism.

## Sequencing

A reasonable ordering, parallelising where possible:

```
drop-reentrant-mutex.md  ──┐
                           ├──► Stage 1 (build config) ──► Stage 4 (tagged userdata) ──► Stage 5 (atom namecall) ──┐
                           └──► Stage 2 (async)        ──► Stage 3 (modern idioms)                                 ├──► Stage 7 (polish)
                                                                                                                   │
Stage 6 (ruau-luau-src + ruau-analyze) ─── runs in parallel from day one ──────────────────────────────────────────┘
```

Stage 6 is independent of stages 1-5 because the analyzer doesn't share
much code with the runtime *yet* — the shared types work only matters once
both crates have settled APIs. Start the `ruau-luau-src` fork and the
`ruau-analyze` skeleton early; do the cross-pollination (shared `Require`,
schema-driven userdata) after stages 1-5 stabilize.

## Cost estimate

With drop-reentrant-mutex.md as the prerequisite (estimate ~2 weeks on its
own), the work above is roughly:

- Stage 1: 1 week
- Stage 2: 2-3 weeks
- Stage 3: 1 week
- Stage 4: 2-3 weeks (largest delta below the analyzer)
- Stage 5: 1 week
- Stage 6: 4-6 weeks (`ruau-luau-src` fork + analyzer port + shared types)
- Stage 7: 2 weeks

**Total: 14-18 weeks (~3.5-4.5 months) of focused work** on top of the
already-merged simplifications, for one engineer. The fork-from-mlua start
saves the ~6 months of greenfield-equivalent infrastructure work. Stage 6
is where the project becomes its own thing rather than "smaller mlua."

## Risks

1. **Upstream Luau churn.** `ruau-luau-src` ownership means we track Luau
   releases ourselves. The cadence is monthly-ish; the existing
   `luau-update-playbook.md` from `luau-analyze` is the template.
2. **Tagged userdata refactor surface area.** Stage 4 touches every
   `T: UserData` impl in the test suite. Plan for a week of test breakage
   while the new contract beds in.
3. **Async-only execution is a public API break.** Anyone depending on
   sync `call`/`exec` will break. Document the mlua migration path in
   release notes.
4. **PR to `luau0-src` may be rejected.** Don't block on it. Treat the
   fork as the primary path, the PR as goodwill.
5. **Stage 6 timing.** The cross-pollination (shared `Require`,
   schema-driven userdata registration) is the most novel work and the
   hardest to estimate. Time-box it; if it overruns, ship the analyzer
   side without the integration, then iterate.

## Open questions

> ASK: Should the analyzer crate be `ruau-analyze` or just folded into
> `ruau` behind a feature like `analyze`? A separate crate keeps build
> times sensible for runtime-only consumers (Analysis is 3.2 MB of C++);
> a feature would simplify the workspace at the cost of forced compile
> work for users who don't want it.

> ASK: Do we accept that `ruau` and `mlua` cannot coexist in the same
> binary (both set `links = "luau"` for `ruau-sys`, vs `links = "lua"` for
> `mlua-sys` — actually different, so they may coexist; verify before
> assuming a hard incompatibility)?

> ASK: For the `Require` trait sharing, is there value in publishing it
> as a separate `ruau-core` crate (analyzer + runtime depend on it
> independently), or do we accept that `ruau-analyze` depends on `ruau`?
