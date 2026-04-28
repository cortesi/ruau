# ruau Next Steps

This repository has already made the right first move: it is a Luau-only fork of
`mlua`, not a new binding layer from scratch. The remaining work is to stop
treating Luau as one backend among many and make the native Luau tree, runtime
API, typechecker API, resolver model, and async story one coherent project.

## Current State

`ruau` is already substantially narrowed:

- The public crate is `ruau`, with a high-level runtime API inherited from
  `mlua`.
- The sys crate is `ruau-sys`, and it links only Luau.
- Runtime-specific features are already Luau-shaped: `luau-jit`,
  `luau-vector4`, `Compiler`, `Require`, sandboxing, interrupts, heap dumps,
  coverage, module registration, and integer64 support.
- The vendored build still comes from `luau0-src = "0.20.0"`.
- The high-level runtime still carries upstream `mlua` structure: `Lua`,
  cloneable handles, `XRc<ReentrantMutex<RawLua>>`, `LuaGuard`, `MaybeSend`,
  `MaybeSync`, `send`, runtime-agnostic async futures, and many `mlua_*`
  internal names.

There are two existing plans worth keeping:

- `plans/drop-reentrant-mutex.md` is the most important runtime cleanup. It
  moves the VM toward single-owner semantics and removes a large amount of
  complexity that exists because `mlua` has to support many runtimes and sharing
  modes.
- `plans/luau.md` is a good deferred naming and cleanup pass. It should follow
  behavioral changes rather than block them.

## Core Recommendation

Keep specializing the `mlua` fork. Do not start a greenfield binding unless the
fork becomes harder to simplify than to replace.

Do not bring `luau-analyze` features in by extending the published `luau0-src`
crate as the long-term integration point. Instead, make this repository own the
vendored Luau source and build rules, either directly inside `ruau-sys` or
through a workspace crate such as `ruau-luau-src`.

The practical recommendation is:

1. Copy/adapt the useful parts of `luau0-src::Build` into this repository.
2. Replace the external `luau0-src` dependency with a repo-owned vendored Luau
   source tree.
3. Extend that repo-owned build to compile the Luau Analysis, Config, Require,
   and selected CLI support sources needed by `luau-analyze`.
4. Port the existing `luau-analyze` C ABI shim and Rust wrapper into this
   workspace.

That gives `ruau` one Luau version, one native build, one symbol strategy, and
one place to expose both runtime and analysis support.

## Why Not Extend `luau0-src`

`luau0-src` is useful as a runtime/compiler vendoring helper. It is not the
right abstraction boundary for this project once analysis is in scope.

The analyzer needs more than the VM and compiler:

- `Analysis`
- `Config`
- `Require`
- AST and parser pieces already used by runtime compilation paths
- CLI-adjacent resolver support such as `AnalyzeRequirer`, `FileUtils`, and
  `VfsNavigator`
- a stable crate-owned C ABI shim for diagnostics, module resolution,
  cancellation, definition loading, and schema extraction

Putting that into the published `luau0-src` crate would couple `ruau` to that
crate's release cadence and API choices. It would also mix analyzer policy with
a source-vendoring helper. This repository needs tighter control than that.

It is fine to begin by copying the `luau0-src` build logic. The long-term state
should be that `ruau-sys` or a workspace vendoring crate owns the Luau source
snapshot/submodule and all native build decisions.

## Native Build Plan

The current build path is:

```text
ruau-sys build.rs
  -> luau0_src::Build
  -> print Cargo metadata
```

Replace it with:

```text
ruau-sys build.rs
  -> ruau-owned Luau build
  -> compile runtime/compiler sources
  -> optionally compile codegen
  -> optionally compile analysis support
  -> compile crate-owned C ABI shims
  -> print Cargo metadata
```

The owned build should compile these Luau components first:

- `Common`
- `Ast`
- `VM`
- `Compiler`
- `Config`
- `Require`
- optional `CodeGen` behind `luau-jit`

Then add analysis support behind an `analysis` feature:

- `Analysis`
- resolver/helper sources currently used by `luau-analyze`
- the existing `luau-analyze` shim, adapted to this workspace

Keep C++ on the native side. Rust should bind a stable C ABI that this project
controls, not Luau's internal C++ API directly.

The existing feature mapping should remain recognizable:

- `luau-jit` controls native codegen.
- `luau-vector4` controls `LUA_VECTOR_SIZE`.
- a new `analysis` feature controls native analysis compilation.
- a generated or checked-in version constant should report the vendored Luau
  tag/commit so runtime and analyzer diagnostics can be traced to the exact
  source revision.

## Where Analysis Should Live

Add a separate high-level crate first:

```text
crates/ruau-analysis
```

This crate should depend on `ruau-sys` and expose the high-level API currently
provided by `luau-analyze`, adjusted to share types and conventions with
`ruau`.

Later, `ruau` can re-export it behind a feature:

```rust
pub mod analysis;
```

Starting as a separate crate is cleaner because analysis has different compile
time, native link, and dependency characteristics from the embedded runtime.
It also avoids forcing every runtime user to compile Luau Analysis.

## Features To Bring From `luau-analyze`

Port the user-facing analysis concepts, not the exact crate layout:

- `Checker`
- `CheckerOptions`
- `CheckOptions`
- `CheckResult`
- `Diagnostic`
- `Severity`
- `CancellationToken`
- virtual modules / in-memory file resolver support
- module schema and entrypoint schema extraction
- definition loading
- path/module resolver configuration

The first target should be feature parity for the workflows this repository
actually needs:

1. Check a source string.
2. Check a file tree using a resolver.
3. Provide virtual files/modules.
4. Load host definitions.
5. Return structured diagnostics with spans.
6. Extract exported type/schema information where `luau-analyze` already can.

Do not expose raw Analysis internals early. Keep the Rust API narrow around
checking, diagnostics, definitions, and module metadata.

## Unifying Runtime Require And Analyzer Resolution

This is the most important integration opportunity beyond what `mlua` provides.

Today the runtime side has:

- `Lua::create_require_function`
- `FsRequirer`
- `Require`
- Luau's `luarequire` integration

The analyzer side from `luau-analyze` has its own resolver/checker machinery.

In `ruau`, these should converge around one Rust-facing resolver model:

```rust
trait ModuleResolver {
    fn resolve(&self, from: ModuleId, specifier: &str) -> Result<ResolvedModule>;
    fn read(&self, module: &ResolvedModule) -> Result<ModuleSource>;
}
```

Runtime `require` and analysis module loading can then be backed by the same
implementation. That unlocks check-then-run behavior where the code that passes
analysis is the code the VM later loads.

Concrete work:

- Extract the current runtime `Require`/`FsRequirer` concepts into a resolver
  abstraction that can serve both runtime and analysis.
- Port the `luau-analyze` resolver shim to call that abstraction.
- Add an in-memory resolver for tests and embedding.
- Add a filesystem resolver that matches Luau's expected require semantics.
- Make diagnostics report the same module IDs and paths that runtime loading
  uses.

## Host API Definitions

The biggest Luau-specific win is connecting Rust host bindings to Luau type
definitions.

`mlua` cannot do this generically because Lua 5.1/5.2/5.3/5.4/LuaJIT do not
share Luau's type system. `ruau` can.

Add a host API metadata layer that records enough information to generate or
load `.d.luau` definitions for Rust-provided globals, functions, tables, and
userdata.

Possible API shape:

```rust
let host = HostApi::new()
    .global_function("log", log_fn, "((string) -> ())")
    .userdata::<Widget>("Widget", widget_defs);

host.install(&lua)?;
checker.add_definitions(host.definitions())?;
```

This does not need to be perfect at first. Even explicit definition strings
attached to runtime registration points would be valuable because they keep
runtime and analyzer configuration together.

Later extensions:

- Generate definitions from Rust traits or derive macros.
- Feed known userdata/global metadata into `Compiler` options where Luau can
  use it.
- Validate that runtime exports and definition exports stay in sync.
- Support schema extraction for plugin/agent boundaries.

## Checked Execution API

Once runtime resolution and analysis resolution share a model, add a checked
loading path:

```rust
let result = checker.check_module("main")?;
if result.has_errors() {
    return Err(result.into_error());
}

let chunk = lua.checked_load(&checker, "main")?;
chunk.call_async::<()>(()).await?;
```

The first implementation can be simple:

- analysis runs first;
- diagnostics are returned without mutating VM state;
- runtime compilation/loading only happens if analysis succeeds;
- source text is obtained from the same resolver used during checking.

This is where `ruau` becomes meaningfully more than `mlua` plus a separate
checker crate.

## Async And Ownership Direction

The existing `plans/drop-reentrant-mutex.md` should be executed before deep
async redesign. It removes the largest inherited constraint from `mlua`.

Recommended ownership target:

- `Lua` is movable but not cloneable.
- `Lua` is not `Sync`.
- VM access does not require a reentrant mutex.
- handles are tied to the owning VM lifetime/identity.
- cross-thread use, if supported later, goes through an explicit actor/handle.

Recommended async target:

- keep async support, but eventually make it Tokio-shaped rather than
  runtime-agnostic;
- consider making async the primary public API;
- remove the `send` feature split once the ownership model is clear;
- do not allow borrowed Lua stack/value references to cross `.await`;
- initially keep async callbacks explicit and conservative;
- avoid async metamethods until the basic callback and cancellation story is
  stable.

Tokio-only async is a reasonable simplifying assumption, but it should not be
the first refactor. The first refactor is removing cloneable shared VM state.
After that, Tokio-specific scheduling and cancellation choices will be much
clearer.

## Cancellation And Timeouts

Unify runtime and analyzer cancellation concepts.

Runtime already has Luau interrupts. `luau-analyze` already has cancellation
support. `ruau` should expose one cancellation type that can be used for both:

```rust
let cancel = CancellationToken::new();
checker.check_with_cancel(module, cancel.clone())?;
lua.set_interrupt(cancel.interrupt())?;
```

If the project commits to Tokio, this can later wrap or interoperate with
`tokio_util::sync::CancellationToken`. Do not make that a hard dependency until
the native and high-level APIs are stable.

## Symbol Strategy

The symbol hiding in `luau-analyze` exists because it must coexist with `mlua`.
If `ruau` replaces `mlua` in this project, that specific constraint goes away.

However, the repo should still avoid accidental duplicate Luau copies in
downstream applications. Owning the vendored native build helps because runtime
and analysis link through the same `ruau-sys` artifact.

Recommended stance:

- one native Luau build per `ruau-sys`;
- runtime and analysis both use it;
- do not hide symbols merely to coexist with `mlua`;
- keep exported shim symbols crate-prefixed;
- avoid exposing Luau C++ symbols as part of the Rust API contract.

## Simplifying Assumptions Worth Taking

These assumptions would materially reduce complexity:

- Luau only.
- Vendored Luau only.
- No system Luau discovery.
- No C module loading mode.
- No support for multiple Lua runtimes in one crate.
- Tokio-only async eventually.
- Async-first public examples and docs.
- No borrowed Lua values across `.await`.
- No async metamethods initially.
- No scoped non-`'static` callbacks in the first cleaned-up async API.
- No raw stack manipulation in the high-level API.
- JIT support remains optional and can lag the interpreter/typechecker path.
- Analyzer support is feature-gated and isolated from minimal runtime builds.

The most valuable simplification is Luau-only ownership of native source and
build rules. The second most valuable is single-owner VM semantics.

## Staged Plan

### Stage 1: Stabilize The Fork Shape

- Keep `plans/drop-reentrant-mutex.md` as the next runtime refactor.
- Run the current test suite before changing the native build.
- Remove or quarantine stale generic Lua concepts only when they block real
  work.
- Do not spend a large pass on renaming before the native build and analyzer
  integration direction is settled.

### Stage 2: Own The Luau Source Build

- Add a repo-owned vendored Luau source tree or `crates/ruau-luau-src`.
- Copy/adapt the `luau0-src` build logic.
- Remove `luau0-src` from `crates/ruau-sys/Cargo.toml`.
- Preserve current runtime behavior and feature flags.
- Add a Luau source version constant.
- Verify existing runtime tests pass.

This stage should intentionally not add analyzer functionality yet. It proves
that the project can build and link its own Luau.

### Stage 3: Add Native Analysis Support

- Add an `analysis` feature to `ruau-sys`.
- Compile the required Luau Analysis and helper sources.
- Port the `luau-analyze` C ABI shim into `ruau-sys`.
- Prefix shim symbols with `ruau_`.
- Keep the shim API narrow: create checker, configure resolver, check module,
  read diagnostics, extract schema, destroy resources.

### Stage 4: Add `crates/ruau-analysis`

- Port the safe Rust wrapper from `luau-analyze`.
- Depend on `ruau-sys`.
- Add structured diagnostics and span types.
- Add string, virtual-file, and filesystem checking tests.
- Keep the first API independent from `Lua`; integration comes next.

### Stage 5: Share Module Resolution

- Extract or redesign resolver traits so runtime `require` and analysis use the
  same module IDs and source lookup.
- Adapt `FsRequirer` to implement the shared resolver.
- Add an in-memory resolver.
- Add tests that check and run the same module graph.

### Stage 6: Add Host Definitions

- Add explicit definition registration alongside runtime registration.
- Feed definitions into `ruau-analysis`.
- Add tests where Rust-provided globals/userdata are visible to the checker and
  then used at runtime.

### Stage 7: Add Checked Runtime Loading

- Add a `checked_load` or `checked_chunk` API.
- Ensure failed analysis does not mutate runtime state.
- Ensure runtime source comes from the same resolver snapshot the checker saw.
- Add integration tests for diagnostics, successful execution, and module
  graphs.

### Stage 8: Rework Async Around The New Ownership Model

- Complete the reentrant mutex removal first.
- Decide whether `async` remains a feature or becomes the primary API.
- Decide when to collapse `send`/non-`send` variants.
- Introduce Tokio-specific helpers only where they remove real complexity:
  cancellation, task-local execution, async module loading, and actor handles.

## Risks

- Luau Analysis native build size and compile time will increase significantly.
- The analyzer uses C++ APIs that are less stable than the plain C runtime API,
  so the C ABI shim must remain owned and tested by this repo.
- Sharing module resolution between runtime and analysis is design-heavy but
  worth doing. Duplicating resolver behavior would make checked execution
  unreliable.
- Async cleanup can easily sprawl. Keep it sequenced behind VM ownership
  cleanup.

## Do Not Do

- Do not extend the published `luau0-src` crate as the long-term integration
  point for analysis.
- Do not bind Luau Analysis C++ types directly in public Rust APIs.
- Do not redesign async, native vendoring, and analysis integration in one
  patch.
- Do not preserve `mlua` compatibility as a goal. Preserve useful API shape,
  tests, and ergonomics, but specialize the internals.
- Do not make analysis a mandatory dependency for minimal runtime users.

## Immediate Next Patch

The next concrete implementation patch should replace the external
`luau0-src` dependency with a repo-owned Luau source/build crate while keeping
runtime behavior unchanged.

That patch should touch roughly:

- `crates/ruau-sys/Cargo.toml`
- `crates/ruau-sys/build/*`
- a new vendored source location or `crates/ruau-luau-src`
- workspace metadata if a new crate is added

It should not yet port `luau-analyze`. Once the native build is owned locally,
analysis support becomes a straightforward extension of the same build instead
of a cross-crate coordination problem.
