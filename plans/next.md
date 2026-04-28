# Next

## Decisions

- Own the Luau source build in this workspace. Add `crates/ruau-luau-src` as a
  repo-owned fork/adaptation of `luau0-src`, and remove the external
  `luau0-src` build dependency from `ruau-sys`.
- Keep `ruau-sys` as the only crate that compiles and links native Luau code.
  Runtime, compiler, CodeGen, Analysis, CLI helper sources, and project-owned C
  ABI shims are all built through the same `links = "luau"` artifact.
- Do not compile Analysis from `ruau-analyze` directly. `ruau-analyze` is a safe
  Rust wrapper over `ruau-sys` with the `analysis` feature enabled.
- Add a small `ruau-core` crate for shared Rust concepts that must be used by
  both runtime and analyzer without creating a dependency cycle.
- Name the analyzer crate `ruau-analyze`, not `ruau-analysis`.
- Keep analysis optional for runtime-only users. A plain `ruau` build must not
  compile Luau Analysis.
- Treat the lock-removal work as landed. Track remaining lock-free cleanup as
  ordinary runtime cleanup; do not block analyzer work on another mutex plan.
- Collapse build variance after the owned native build is stable: vector is
  3-wide only, CodeGen is always compiled, and JIT is controlled at runtime.
- Move toward Tokio-only async and async-first execution, but sequence that after
  native ownership and analyzer integration.
- Keep public Rust APIs narrow around checking, diagnostics, definitions,
  module resolution, and checked loading. Do not expose Luau C++ types.

## Stage 1: Own The Luau Source Build

Goal: replace `luau0-src` with a workspace-owned build while preserving current
runtime behavior.

1. [ ] Add `crates/ruau-luau-src`.
2. [ ] Copy/adapt the useful parts of `luau0_src::Build`.
3. [ ] Vendor or submodule the Luau source snapshot in the new crate.
4. [ ] Expose a build API with at least:
   - `enable_codegen(bool)`
   - `enable_analysis(bool)`
   - `set_vector_size(usize)` while the existing feature still exists
   - `set_max_cstack_size(usize)`
   - a generated or checked-in Luau version constant
5. [ ] Replace `luau0-src = "0.20.0"` in `crates/ruau-sys/Cargo.toml`.
6. [ ] Update `crates/ruau-sys/build/find_vendored.rs` to call the local build
   helper.
7. [ ] Preserve `luau-codegen` and `luau-vector4` behavior for this stage.
8. [ ] Verify default and all-feature runtime tests pass.

This stage is intentionally a behavioral no-op. It proves this workspace owns
the native build before adding analysis or removing feature variance.

## Stage 2: Add Native Analysis To `ruau-sys`

Goal: compile Luau Analysis and the analyzer C ABI through the single native
artifact.

1. [ ] Add an `analysis` feature to `ruau-sys`.
2. [ ] Extend `ruau-luau-src` to compile Luau's `Analysis` sources when
   analysis is enabled.
3. [ ] Include the required Luau helper sources used by `luau-analyze`, such as
   `AnalyzeRequirer.cpp`, `FileUtils.cpp`, and `VfsNavigator.cpp`.
4. [ ] Port the existing `luau-analyze` C ABI shim into `ruau-sys`.
5. [ ] Prefix all shim symbols with `ruau_`.
6. [ ] Keep the shim API narrow:
   - create and destroy checker/front-end state
   - configure definitions
   - configure resolver callbacks
   - check module or source text
   - read structured diagnostics
   - extract entrypoint and module schemas
   - cancel in-flight work
7. [ ] Add native build tests that prove runtime-only and analysis-enabled
   builds do not produce duplicate Luau symbols.

## Stage 3: Add `ruau-core`

Goal: define shared Rust contracts without making `ruau` and `ruau-analyze`
depend on each other.

1. [ ] Add `crates/ruau-core`.
2. [ ] Keep it Rust-only. It must not depend on `ruau`, `ruau-sys`, or
   `ruau-analyze`.
3. [ ] Move or introduce shared types:
   - `ModuleId`
   - `ResolvedModule`
   - `ModuleSource`
   - `ModuleResolver`
   - `CancellationToken`
   - diagnostic span/range types
   - host definition data structures
4. [ ] Keep errors lightweight in `ruau-core`; conversion into `ruau::Error` or
   `ruau_analyze::Error` belongs in the outer crates.
5. [ ] Make `ruau` depend on `ruau-core`.
6. [ ] Adapt the current runtime `Require`/`FsRequirer` path toward the shared
   resolver traits without changing runtime behavior yet.

## Stage 4: Add `ruau-analyze`

Goal: port the safe analyzer wrapper as a workspace crate.

1. [ ] Add `crates/ruau-analyze`.
2. [ ] Depend on `ruau-core` and `ruau-sys` with `features = ["analysis"]`.
3. [ ] Replace the old libloading-based FFI layer with plain `extern "C"`
   declarations against `ruau-sys`.
4. [ ] Port the user-facing analyzer API:
   - `Checker`
   - `CheckerOptions`
   - `CheckOptions`
   - `CheckResult`
   - `Diagnostic`
   - `Severity`
   - virtual modules and in-memory files
   - definition loading
   - module and entrypoint schema extraction
5. [ ] Port `module_schema.rs` as pure Rust code.
6. [ ] Port analyzer tests and fixtures.
7. [ ] Add first-class tests for:
   - checking a source string
   - checking a filesystem tree
   - checking virtual files
   - loading definitions
   - returning diagnostics with stable spans
   - extracting module and entrypoint schema

## Stage 5: Share Module Resolution

Goal: make checked code and executed code resolve the same modules.

1. [ ] Replace analyzer-only resolver concepts with `ruau-core::ModuleResolver`.
2. [ ] Adapt runtime `require` to use the shared resolver model.
3. [ ] Add an in-memory resolver for tests and embedding.
4. [ ] Add a filesystem resolver that matches Luau `require` semantics.
5. [ ] Make diagnostics report the same module IDs and paths that runtime
   loading uses.
6. [ ] Add tests that check and run the same module graph from one resolver.

## Stage 6: Add Host Definitions

Goal: keep runtime registration and analyzer definitions together.

1. [ ] Add explicit definition registration alongside runtime registration.
2. [ ] Start with explicit `.d.luau` strings attached to globals, functions,
   tables, and userdata.
3. [ ] Feed registered host definitions into `ruau-analyze`.
4. [ ] Add tests where Rust-provided globals and userdata are visible to the
   checker and then used successfully at runtime.
5. [ ] Later, add generation from Rust traits or derive macros only where it
   removes real duplication.

Example target shape:

```rust
let host = HostApi::new()
    .global_function("log", log_fn, "((string) -> ())")
    .userdata::<Widget>("Widget", widget_defs);

host.install(&lua)?;
checker.add_definitions(host.definitions())?;
```

## Stage 7: Add Checked Runtime Loading

Goal: expose the integration point that makes the analyzer part of the runtime
workflow.

1. [ ] Add a `checked_load` or `checked_chunk` API.
2. [ ] Run analysis before mutating VM state.
3. [ ] Return diagnostics if analysis fails.
4. [ ] Compile/load into the VM only when analysis succeeds.
5. [ ] Read source from the same resolver snapshot used during checking.
6. [ ] Add integration tests for diagnostics, successful execution, and module
   graphs.

Example target shape:

```rust
let result = checker.check_module("main")?;
if result.has_errors() {
    return Err(result.into_error());
}

let chunk = lua.checked_load(&checker, "main")?;
chunk.call_async::<()>(()).await?;
```

## Stage 8: Collapse Build And API Variance

Goal: remove inherited flexibility that no longer earns its keep.

1. [ ] Remove `luau-vector4`; vectors are 3-wide.
2. [ ] Make CodeGen always compile.
3. [ ] Replace the `luau-jit` build feature with runtime control such as
   `Lua::enable_jit(bool)`, defaulting to `false`.
4. [ ] Remove stale `send` and non-`send` cfg branches.
5. [ ] Collapse `MaybeSend` and `MaybeSync`.
6. [ ] Remove `userdata-wrappers` after the tagged-userdata design replaces the
   inherited wrapper path.
7. [ ] Remove `error-send` if all public errors can be `Send + Sync`
   unconditionally.
8. [ ] Remove the deprecated `serialize` feature alias.
9. [ ] Keep `async`, `serde`, `macros`, and `anyhow` until their final public
   shape is decided.

## Stage 9: Tokio-First Async

Goal: simplify async around one scheduler model.

1. [ ] Decide whether `async` remains a feature or becomes unconditional.
2. [ ] Replace runtime-agnostic async plumbing with Tokio primitives where they
   remove real complexity.
3. [ ] Use shared cancellation for analyzer checks and runtime interrupts.
4. [ ] Keep sync Rust callbacks for cheap host functions.
5. [ ] Make Luau execution async-first:
   - `Function::call(...).await`
   - `Chunk::exec(...).await`
   - `Chunk::eval(...).await`
6. [ ] Do not allow borrowed Lua stack/value references to cross `.await`.
7. [ ] Avoid async metamethods until basic callbacks, cancellation, and module
   loading are stable.
8. [ ] Add Tokio helper installation as an explicit opt-in, not default global
   behavior.

## Stage 10: Runtime Performance Refactors

Goal: spend large runtime refactors only after the analyzer path is usable.

1. [ ] Replace inherited userdata storage with per-type Luau userdata tags.
2. [ ] Use `lua_newuserdatataggedwithmetatable` for `T: UserData`.
3. [ ] Bind metatables per tag with `lua_setuserdatametatable`.
4. [ ] Register Rust drops per tag with `lua_setuserdatadtor`.
5. [ ] Make `FromLua for T` check the Luau userdata tag and cast directly.
6. [ ] Rework scoped userdata around tag-local destructed sentinels.
7. [ ] Add atom-based `__namecall` dispatch using Luau's atom callback and
   `lua_namecallatom`.
8. [ ] Benchmark namecall-heavy and userdata-heavy cases before keeping the
   refactors.

## Validation

- Run `cargo xtask test` after each stage that changes Rust or native build
  behavior.
- Run a default-feature build and an all-feature build whenever feature edges
  change.
- Add focused tests before broad refactors:
  - native symbol duplication
  - analysis-enabled linking
  - virtual-module diagnostics
  - resolver parity between check and run
  - checked loading without VM mutation on failure
  - host definitions visible to analysis and runtime
- Keep public examples current with the chosen API shape.

## Do Not Do

- Do not extend the published `luau0-src` crate as the long-term integration
  point.
- Do not vendor only Analysis beside `luau0-src`; that creates two Luau pins.
- Do not build native Luau code from both `ruau-sys` and `ruau-analyze`.
- Do not bind Luau C++ APIs directly in public Rust APIs.
- Do not make analysis mandatory for runtime-only users.
- Do not preserve `mlua` compatibility as a design goal.
- Do not redesign async, native vendoring, analysis integration, and userdata in
  one patch.

## Immediate Patch

Replace the external `luau0-src` dependency with `crates/ruau-luau-src` while
keeping runtime behavior unchanged.

Expected touch points:

- `Cargo.toml`
- `crates/ruau-sys/Cargo.toml`
- `crates/ruau-sys/build/find_vendored.rs`
- `crates/ruau-luau-src/**`

Do not port `luau-analyze` in the immediate patch. Native ownership comes first;
Analysis becomes a feature of the same build after that is stable.
