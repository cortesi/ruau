# Modernization Recommendations

This plan captures post-refactor recommendations for making `ruau` a smaller, more idiomatic,
Tokio-first Luau embedding crate. The ordering is intentional: low-regret correctness fixes and
obvious cleanup come first; larger API choices, new dependencies, and surface-area expansion are
deferred until the existing shape is clearer.

Research inputs:

- Luau type modes, lints, type functions, `.config.luau`, require-by-string, vector, buffer,
  CodeGen, and userdata-tag updates from `luau.org` and `rfcs.luau.org`.
- Current crate state in `crates/ruau/src`, especially `analyzer.rs`, `resolver.rs`, `host.rs`,
  `luau/require`, `stdlib.rs`, and `chunk.rs`.
- Ecosystem crates to evaluate later: `bitflags`, `camino`, `tokio-util`, and optional diagnostic
  integration through `miette` or `codespan-reporting`.

1. Stage One: Immediate Correctness And Documentation

Fix known defects and stale docs before changing API shape. These items are useful regardless of
which larger design choices land later.

1. [x] Fix `StdLib::ALL_SAFE`: it currently equals `u32::MAX`, which includes `DEBUG`; define safe
   libraries explicitly and decide whether `OS` belongs in safe defaults for Luau embeddings.
2. [x] Fix rustdoc broken links in `crates/ruau/src/analyzer.rs`; `cargo doc -p ruau --no-deps
   --all-features` currently reports unresolved links for `Checker`, `Diagnostic`,
   `Checker::check`, `Checker::check_path`, and `CheckOptions::virtual_modules`.
3. [x] Remove docs that link ordinary runtime concepts to Lua 5.2 or Lua 5.4 manuals when Luau docs
   or vendored Luau headers are the real authority.
4. [x] Remove stale compatibility labels in examples and tests such as "Lua" comments for Luau-only
   functionality, except when deliberately testing `.lua` filename compatibility.
5. [x] Add crate-level rustdoc sections for checked loading, resolver snapshots, host definitions,
   Tokio local execution, and Luau-only feature choices.
6. [x] Update `README.md`, guided tour, and examples to stop presenting the crate as a Lua-family
   binding with Luau support; present it as a Luau embedding toolkit.

2. Stage Two: Low-Regret Lua-Era Cleanup

Remove or hide compatibility residue that is already misleading now that the crate is Luau-only.

1. [x] Confirm no root-level `String = LuauString` alias remains; callers use `LuauString`
   explicitly when they need a VM string handle.
2. [x] Confirm no root-level `BString` re-export remains; callers import `bstr::BString` directly
   while `ruau` keeps conversion impls for it.
3. [x] Confirm no `unsafe_new` or `unsafe_new_with` APIs remain; safe construction controls standard
   library loading through `StdLib`.
4. [x] Remove `register_module`/`unload_module` package-style injection with `@` names and
   lowercasing; resolver snapshots and Luau's require cache are the supported module path.
5. [x] Make binary bytecode loading an explicit unsafe opt-in instead of exposing
   `ChunkMode::Binary` through an ordinary safe setter.
6. [x] Review root re-exports in `crates/ruau/src/lib.rs`; no duplicate analyzer/resolver paths are
   exported at the root.

3. Stage Three: Existing API Ergonomics

Improve APIs already in the crate without adding new capabilities or new external integrations.

1. [x] Remove unnecessary `Send` bounds from `HostApi` installers and function closures in
   `crates/ruau/src/host.rs`; `Luau::create_function` and `create_async_function` are local VM APIs
   and should allow local captures.
2. [x] Split `HostApi` into an immutable definition bundle and repeatable runtime installer, or make
   `install(&self, &Luau)` possible, so one host description can be installed into multiple VMs and
   loaded into multiple checkers.
3. [x] Rename consuming builder methods on `Compiler` from `set_*` to fluent names such as
   `optimization_level`, `debug_level`, `coverage_level`, `mutable_globals`, and
   `disabled_builtins`.
4. [x] Replace numeric `u8` compiler option levels with small enums when the valid values are fixed
   and invalid values are rejected by Luau.
5. [x] Prefer specific constructors over public option structs for states that can be invalid, but
   keep struct literals where all fields are simple data.
6. [x] Review `Function::call`, `Chunk::exec`, `Chunk::eval`, and `Thread::into_async` docs for
   current-thread assumptions and make non-`Send` future behavior explicit.
7. [x] Add a small Tokio embedding example that shows `LocalSet` when users spawn local Luau tasks,
   while keeping the core VM type `Send + !Sync`.

4. Stage Four: Consolidate Existing Module Resolution

Unify duplicate resolver concepts before adding async resolution, richer diagnostics, or new
resolver dependencies.

1. [x] Make `crates/ruau/src/resolver.rs` the canonical public module-resolution API.
2. [x] Rename or remove the public `ruau::luau` module. Once the whole crate is Luau-only, a
   "Luau-specific extensions" namespace is redundant.
3. [x] Hide or delete `ruau::Require`, `ruau::FsRequirer`, and `Luau::create_require_function`
   unless there is a concrete embedder use case that cannot be represented by `ModuleResolver`.
4. [x] Replace `ResolverSnapshot::resolve`'s `string_requires` scanner with Luau-native require
   discovery, either through `RequireTracer`, an AST-backed shim, or an owned parser wrapper.
5. [x] Keep `FilesystemResolver` deliberately config-free: aliases, ancestry-based config merging,
   `.config.luau`, and `.luaurc` are integrating-application policy implemented through a custom
   `ModuleResolver`, not crate-level filesystem behavior.
6. [x] Decide `.lua` file policy explicitly. Luau's tooling still recognizes `.lua`, so either keep
   `.lua` for require parity or make `.luau`-only a documented crate-level breaking choice.
7. [x] Ensure `Luau::checked_load`, ordinary runtime `require`, and `Checker::check_snapshot` all
   consume the same `ModuleResolver` semantics and `ModuleId` cache keys.

5. Stage Five: Small Luau-Native Improvements

Add Luau-specific functionality that is narrow, obvious, and directly supported by the current
runtime.

1. [ ] Expand `Vector` conversions and ergonomics: `From<[f32; 3]>`, `From<Vector> for [f32; 3]`,
   serde round-trips, and compiler constants that assume the built-in `vector` library.
2. [ ] Reassess hidden `Compiler::vector_ctor` and `vector_type`; the built-in vector
   library now makes custom constructor/type shims less central.
3. [ ] Expand `Buffer` beyond raw byte copying with checked typed reads/writes that map cleanly to
   the existing Luau buffer API.
4. [ ] Add bit-level `Buffer` helpers that mirror Luau's `buffer.readbits` and `buffer.writebits`
   behavior.
5. [ ] Add coverage and heap/memory-category examples that are Luau-specific rather than inherited
   Lua debug API examples.
6. [ ] Audit `ruau-sys` bindings after each Luau source bump for new C APIs around require,
   CodeGen, buffers, userdata tags, coverage, and analyzer options.

6. Stage Six: Ecosystem Crate Decisions

Evaluate dependencies only after the local cleanup clarifies what problems remain. Prefer crates
that encode invariants or interoperate with Tokio; avoid convenience dependencies.

1. [ ] Replace the manual `StdLib(u32)` implementation in `crates/ruau/src/stdlib.rs` with
   `bitflags` if the safe/unsafe library policy from Stage One still benefits from flag-set
   semantics.
2. [ ] Consider `camino` for resolver-facing path types because Luau module labels, diagnostics,
   and config paths are UTF-8 text; keep public APIs accepting `impl AsRef<Path>` where callers
   touch the OS.
3. [ ] Consider `tokio-util::sync::CancellationToken` as the public cancellation primitive, with a
   private bridge to the native analyzer token, instead of exposing a bespoke token type only usable
   by `ruau`.
4. [ ] Do not add small-string crates such as `smol_str` unless profiling shows `ModuleId`,
   diagnostic labels, or resolver snapshots are allocation hot spots.
5. [ ] Keep `anyhow` support optional, but do not let application error-reporting patterns leak into
   core public error types.

7. Stage Seven: Larger Surface-Area Decisions

Defer these until the smaller cleanup and resolver consolidation have landed. Each item expands the
project's public surface or makes a larger architectural commitment.

1. [ ] Add an async resolver path for Tokio applications, for example `AsyncModuleResolver` and
   `ResolverSnapshot::resolve_async`, so filesystem or service-backed modules do not require
   blocking reads on the runtime thread.
2. [ ] Add `checked_load_resolved_async` or an equivalent async flow that resolves modules with a
   Tokio-aware resolver before running the synchronous native checker.
3. [ ] Unify analyzer cancellation, runtime interrupt cancellation, and Tokio timeouts in docs and
   examples. If `tokio-util` is adopted, expose conversions rather than two unrelated tokens.
4. [ ] Add analyzer options for language mode, lint configuration, lint-as-error, type-as-error,
   globals, and config-file loading. Map these to Luau's `.config.luau` and `.luaurc` semantics.
5. [ ] Make diagnostics richer: diagnostic code/name when available, lint category, full module
   path, related spans if native Analysis exposes them, and source text integration for reporters.
6. [ ] Add an optional `diagnostics` feature only if it gives concrete value: either
   `miette::Diagnostic` conversions for application reporting or `codespan-reporting` helpers for
   examples and command-line tools. Do not make a reporting crate part of the default embedding API.
7. [ ] Extend schema extraction to understand Luau generics, type packs, exported aliases,
   user-defined type functions, and `types` library constructs where that matters to host APIs.
8. [ ] Add a first-class native CodeGen surface around support detection, per-module compilation,
   compile results, memory limits, and `--!native` behavior instead of only
   `Luau::enable_jit(bool)`.
9. [ ] Add an end-to-end test that uses host definitions, async host functions, async resolution,
   checked loading, cancellation, and execution under `tokio::task::LocalSet`.

8. Stage Eight: Validation And Release Gate

Treat this as a breaking modernization pass with strong public-surface verification.

1. [ ] Run `cargo fmt --all`.
2. [ ] Run `cargo clippy --all --all-targets --all-features --tests --examples`.
3. [ ] Run `cargo test -p ruau --all-features --no-run` before broad test runs to catch API breakage
   quickly.
4. [ ] Run `cargo xtask test`.
5. [ ] Run `cargo doc --workspace --all-features --no-deps` and require zero rustdoc warnings.
6. [ ] Add focused tests for resolver/config parity, host API reuse, buffer bit operations, and
   bytecode safety policy.
7. [ ] Add focused tests for any larger choices accepted in Stage Seven, such as async resolver
   behavior, cancellation bridging, diagnostic rendering, or native CodeGen controls.
8. [ ] Re-run an API-surface review with `ruskel` after the cleanup and ensure every public item has
   exactly one intended import path.
9. [ ] Publish release notes that group changes by "cleanup", "Tokio API", "Luau-native features",
   "ecosystem dependencies", and "removed Lua-era compatibility".
