# Next

## Decisions

- Own the Luau source build in this workspace. Add `crates/ruau-luau-src` as a
  repo-owned fork/adaptation of `luau0-src`, and remove the external
  `luau0-src` build dependency from `ruau-sys`.
- Keep `ruau-sys` as the only crate that compiles and links native Luau code.
  Runtime, compiler, CodeGen, Analysis, and project-owned C ABI shims are all
  built through the same `links = "luau"` artifact.
- Compile Luau Analysis by default in `ruau-sys`. Do not add an `analysis`
  Cargo feature or a runtime-only native build shape.
  This accepts a compile-time and binary-size cost for consumers that never call
  the checker. The tradeoff is intentional: host definitions, shared module
  resolution, diagnostics, and checked loading should all have one native shape
  and one public Rust API instead of a feature-propagated runtime/analyzer
  split.
- Drop the old lazy analyzer packaging. The integrated checker no longer
  extracts or `dlopen`s a private analyzer library on `Checker::new()`, so
  binaries link the native Analysis code up front and analyzer construction no
  longer has an `Error::NativeLibrary` failure mode.
- Do not add a separate `ruau-analyze` crate. The safe analyzer API is an
  integrated `ruau` capability that shares the runtime crate's module resolver,
  host definition, diagnostic, and checked-loading types.
- Do not add `ruau-core` just to avoid a dependency cycle that no longer exists.
  Shared Rust contracts can live in `ruau`; `ruau-sys` stays the raw native
  boundary.
- Treat the runtime consolidation work as landed: vectors are 3-wide, CodeGen is
  always compiled, JIT is controlled at runtime, async is unconditional and
  Tokio-backed, and the old feature variance is gone.
- Treat the lock-removal work as landed. Track remaining lock-free cleanup as
  ordinary runtime cleanup; do not block analyzer work on another mutex plan.
- Keep public Rust APIs narrow around checking, diagnostics, definitions,
  module resolution, and checked loading. Do not expose Luau C++ types.
- Treat `ruau-sys` as an internal native implementation, not as a process-wide
  Lua/Luau provider. Compile native code with hidden visibility where supported
  and avoid exporting accidental `lua_*`, `luaL_*`, `luaC_*`, or `Luau::*`
  symbols from downstream dynamic artifacts.
- Preserve the useful `luau-analyze` behavior contracts while changing the
  packaging: strict mode, new solver, persistent checker definitions,
  deterministic diagnostics, explicit timeout/cancellation, and no batch queue
  API in the first integrated version.

## Stage 1: Own The Luau Source Build

Goal: replace `luau0-src` with a workspace-owned build while preserving current
runtime behavior.

1. [ ] Add `crates/ruau-luau-src`.
2. [ ] Copy/adapt the useful parts of `luau0_src::Build`.
3. [ ] Vendor or submodule the complete Luau source snapshot in the new crate.
   Stage 1 compiles only the current runtime subset; Stage 2 expands the same
   source snapshot to Analysis rather than adding a second source drop.
4. [ ] Expose a build API with at least:
   - always-on CodeGen source compilation
   - fixed 3-wide vector configuration
   - `set_max_cstack_size(usize)`
   - a generated or checked-in Luau version constant
   - source-root and include-path metadata for `ruau-sys` shim compilation
5. [ ] Replace `luau0-src = "0.20.0"` in `crates/ruau-sys/Cargo.toml`.
6. [ ] Update `crates/ruau-sys/build/find_vendored.rs` to call the local build
   helper.
7. [ ] Verify default and all-feature runtime tests pass.

This stage is intentionally a behavioral no-op for the runtime. It proves this
workspace owns the native build before adding Analysis to that build.

## Stage 2: Add Native Analysis To `ruau-sys`

Goal: compile Luau Analysis and the analyzer C ABI unconditionally through the
single native artifact.

1. [ ] Extend `ruau-luau-src` so the default build compiles Luau's `Analysis`
   sources.
2. [ ] Include the native components the old analyzer needed:
   - `Common`
   - `Ast`
   - `VM`
   - `Compiler`
   - `Config`
   - `Analysis`
   - `Require`
3. [ ] Do not compile Luau CLI helper sources such as `AnalyzeRequirer.cpp`,
   `FileUtils.cpp`, or `VfsNavigator.cpp`. Use them only as behavioral
   references where the Rust resolver needs to match Luau CLI semantics.
4. [ ] Port the existing analyzer C ABI shim into `ruau-sys`, but do not treat
   it as a clean lift-and-shift: rewrite file IO and require resolution away
   from `Luau/FileUtils.h` and `Luau/AnalyzeRequirer.h` onto project-owned C
   ABI callbacks in this stage.
5. [ ] Define the low-level C ABI callback table that Luau `FileResolver` and
   `ConfigResolver` adapters use before the public `ModuleResolver` exists.
   Stage 3 will adapt the safe Rust resolver API onto these callbacks.
6. [ ] Compile the shim from `crates/ruau-sys/build/main.rs` with `cc`, using
   the source-root and include-path metadata exported by `ruau-luau-src`.
7. [ ] Prefix all shim symbols with `ruau_`.
8. [ ] Keep the shim API narrow:
   - create and destroy checker/front-end state
   - configure strict mode and the new solver
   - configure definitions
   - configure resolver callbacks
   - check module or source text
   - read structured diagnostics
   - extract entrypoint and module schemas
   - cancel in-flight work
9. [ ] Keep C++ types private to the shim and Luau build. Public Rust APIs must
   see only Rust-owned handles and data structures.
10. [ ] Keep explicit ownership/free functions for every shim-allocated result
   crossing the C ABI.
11. [ ] Add native build tests that prove the default build links runtime,
   CodeGen, Analysis, and the shim.
12. [ ] Add symbol-visibility tests for dynamic artifacts that ensure native
   Luau implementation symbols are not accidentally exported.

## Stage 3: Share Module Resolution Inside `ruau`

Goal: make checked code and executed code resolve the same modules.

1. [ ] Add shared Rust types inside `ruau`:
   - `ModuleId`
   - `ResolvedModule`
   - `ModuleSource`
   - `ModuleResolver`
   - `CancellationToken`
   - diagnostic span/range types
2. [ ] Require resolver implementations and analysis cancellation handles to be
   `Send + Sync + 'static`.
3. [ ] Adapt runtime `require` to use the shared resolver model.
4. [ ] Add an in-memory resolver for tests and embedding.
5. [ ] Add a filesystem resolver that matches Luau `require` semantics.
6. [ ] Preserve the useful old resolver distinction: source-only checks resolve
   exact-name virtual modules, while relative filesystem `require(...)` needs a
   path-backed module label or a path-based check helper.
7. [ ] Make diagnostics report the same module IDs and paths that runtime
   loading uses.
8. [ ] Add tests that check and run the same module graph from one resolver.

Runtime execution cancellation stays on the existing VM interrupt path
(`lua_callbacks(L)->interrupt`) and is not unified with
`CancellationToken` in this stage. `CancellationToken` remains the public Rust
name for analyzer/front-end cancellation after Stage 3; checked loading may use
both mechanisms later, but the resolver model should not pretend they are one
native primitive.

## Stage 4: Add Integrated Analyzer API To `ruau`

Goal: port the safe analyzer wrapper into the runtime crate instead of a
separate analyzer crate.

1. [ ] Add an analyzer module to `crates/ruau`.
2. [ ] Delete the old libloading-based FFI layer and call the analyzer shim
   through `ruau-sys`.
3. [ ] Port the user-facing analyzer API:
   - `Checker`
   - `CheckerOptions`
   - `CheckOptions`
   - `CheckResult`
   - `Diagnostic`
   - `Severity`
   - `CancellationToken`
   - virtual modules and in-memory files
   - definition loading
   - module and entrypoint schema extraction
4. [ ] Replace the old native-library load error with errors that match the
   integrated path: checker creation, resolver callbacks, definitions, check
   execution, timeout, and cancellation.
5. [ ] Port `module_schema.rs` as pure Rust code.
6. [ ] Keep `Checker` reusable and `Send` but not `Sync`; checker methods that
   touch analysis state should require exclusive access and preserve loaded
   definitions across checks.
7. [ ] Keep checker state independent of a live `Lua` VM unless checked loading
   explicitly connects them.
8. [ ] Support default and per-call check options:
   - default source and definitions module labels
   - per-call module label
   - per-call timeout
   - per-call cancellation token
   - per-call virtual modules
9. [ ] Adapt `Checker` to the Stage 3 `ModuleResolver` and resolver snapshots
   instead of adding a second analyzer-only resolution model.
10. [ ] Add path-based helpers that preserve file labels in diagnostics and
   definition errors.
11. [ ] Port analyzer tests and fixtures.
12. [ ] Add first-class tests for:
   - checking a source string
   - checking a filesystem tree
   - checking virtual files through the in-memory resolver
   - loading definitions
   - loading multiple definition files with distinct labels
   - returning diagnostics with stable spans
   - deterministic diagnostic ordering
   - warning-only results
   - syntax errors
   - timeout and cancellation reporting
   - extracting module and entrypoint schema

## Stage 5: Add Host Definitions

Goal: keep runtime registration and analyzer definitions together.

1. [ ] Add explicit definition registration alongside runtime registration.
2. [ ] Start with explicit `.d.luau` strings attached to globals, functions,
   tables, and userdata.
3. [ ] Feed registered host definitions into the integrated checker.
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

## Stage 6: Add Checked Runtime Loading

Goal: expose the integration point that makes the analyzer part of the runtime
workflow.

1. [ ] Add a `checked_load` or `checked_chunk` API that accepts an explicit
   Stage 3 resolver snapshot, not just a module name.
2. [ ] Resolve the target module graph from one resolver snapshot.
3. [ ] Check the entire transitive module graph before mutating VM state.
4. [ ] Return diagnostics without compiling or loading anything if any module in
   the graph fails analysis.
5. [ ] Compile/load into the VM only after the full graph checks successfully.
6. [ ] Read source for compilation from the same resolver snapshot used during
   checking.
7. [ ] Wire analysis timeout/cancellation through the checked-loading path
   before VM mutation; keep runtime interrupt cancellation as a separate VM
   execution concern.
8. [ ] Add integration tests for diagnostics, successful execution, module
   graphs, and a late dependency failure that leaves the VM unmodified.

Example target shape:

```rust
let snapshot = resolver.snapshot("main")?;
let chunk = lua.checked_load(&mut checker, snapshot)?;
chunk.call::<()>(()).await?;
```

`checked_load` must use the supplied snapshot for both analysis and runtime
compilation. Ordinary runtime `require` may still use the resolver registered
on `Lua`, but checked loading should not infer two independent resolvers from
`Lua` and `Checker`.

## Stage 7: Runtime Performance Refactors

Goal: spend large runtime refactors only after the analyzer path is usable.

1. [ ] Add a per-`Lua` userdata tag allocator that respects `LUA_UTAG_LIMIT`.
2. [ ] Use `lua_newuserdatataggedwithmetatable` as a fast path only for
   `T: UserData` types that successfully receive a tag.
3. [ ] Preserve the existing inherited userdata storage as the fallback when
   tags are exhausted or a type opts out of tagging.
4. [ ] Bind metatables per allocated tag with `lua_setuserdatametatable`.
5. [ ] Register Rust drops per allocated tag with `lua_setuserdatadtor`.
6. [ ] Make `FromLua for T` check the Luau userdata tag when a tag exists and
   fall back to the current type check otherwise.
7. [ ] Rework scoped userdata around tag-local destructed sentinels without
   removing the untagged fallback.
8. [ ] Add atom-based `__namecall` dispatch using Luau's atom callback and
   `lua_namecallatom`.
9. [ ] Benchmark namecall-heavy and userdata-heavy cases before keeping the
   refactors.

## Validation

- Run `cargo xtask test` after each stage that changes Rust or native build
  behavior.
- Run a default-feature build and an all-feature build whenever Cargo feature
  edges change.
- Add focused tests before broad refactors:
  - default native build links runtime, CodeGen, and Analysis
  - no accidental native Luau exports from dynamic artifacts
  - Luau CLI helper sources are not compiled into `ruau-sys`
  - strict mode enforced even without `--!strict`
  - new solver policy is fixed
  - no public batch/queue analysis API
  - integrated checker over source strings, filesystem trees, and virtual files
  - filesystem and virtual modules mixed in one require graph
  - multiple definition files with distinct labels
  - stable diagnostic spans
  - deterministic diagnostic ordering
  - timeout and cancellation result flags
  - resolver parity between check and run
  - checked loading without VM mutation on failure
  - host definitions visible to analysis and runtime
  - userdata tag exhaustion falls back to the inherited storage path
- Port the `luau-analyze` example smoke scripts and `.d.luau` fixtures into
  this workspace so expected pass/fail behavior is preserved during migration.
- When bumping Luau, verify the shim against `Frontend.h`, `FileResolver.h`,
  and `ConfigResolver.h`, and confirm `FrontendOptions` still exposes timeout
  and cancellation hooks.
- Keep public examples current with the chosen API shape.

## Do Not Do

- Do not extend the published `luau0-src` crate as the long-term integration
  point.
- Do not vendor only Analysis beside `luau0-src`; that creates two Luau pins.
- Do not build native Luau code from more than one crate.
- Do not compile Luau CLI helper sources into `ruau-sys` unless the Rust
  resolver callback design proves insufficient.
- Do not add an `analysis` Cargo feature to `ruau-sys`.
- Do not add a separate `ruau-analyze` crate unless a later concrete API split
  proves it is worth the extra package boundary.
- Do not add `ruau-core` just to share types between crates that no longer need
  to be separate.
- Do not bind Luau C++ APIs directly in public Rust APIs.
- Do not reintroduce removed runtime feature variance.
- Do not preserve `mlua` API compatibility as a design goal, but do keep native
  symbol visibility disciplined.
- Do not redesign native vendoring, analysis integration, checked loading, and
  userdata storage in one patch.

## Immediate Patch

Replace the external `luau0-src` dependency with `crates/ruau-luau-src` while
keeping runtime behavior unchanged.

Expected touch points:

- `Cargo.toml`
- `crates/ruau-sys/Cargo.toml`
- `crates/ruau-sys/build/find_vendored.rs`
- `crates/ruau-luau-src/**`

Do not port the analyzer C ABI or safe checker API in the immediate patch.
Native ownership comes first; Analysis is added next as a default part of the
same `ruau-sys` build, not as a feature-gated build variant.
