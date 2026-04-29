# Modernization Recommendations

This plan captures post-refactor recommendations for making `ruau` a smaller, more idiomatic,
Tokio-first Luau embedding crate. It assumes clean-break API changes are allowed and focuses on
changes that remove old Lua-era surface area, use ecosystem crates where they encode real
invariants, and expose Luau-specific capabilities that are now central to the project.

Research inputs:

- Luau type modes, lints, type functions, `.config.luau`, require-by-string, vector, buffer,
  CodeGen, and userdata-tag updates from `luau.org` and `rfcs.luau.org`.
- Current crate state in `crates/ruau/src`, especially `analyzer.rs`, `resolver.rs`, `host.rs`,
  `luau/require`, `stdlib.rs`, `chunk.rs`, and `prelude.rs`.
- Current ecosystem crates worth considering: `bitflags`, `camino`, `tokio-util`, and optional
  diagnostic integration through `miette` or `codespan-reporting`.

1. Stage One: Dependency And Surface Policy

Keep dependencies conservative, but stop hand-rolling crates that express the project's domain
better than local code.

1. [ ] Replace the manual `StdLib(u32)` implementation in `crates/ruau/src/stdlib.rs` with
   `bitflags`, including derived `Hash`, `Default` where useful, and explicit unknown-bit policy.
2. [ ] Fix `StdLib::ALL_SAFE`: it currently equals `u32::MAX`, which includes `DEBUG`; define safe
   libraries explicitly and decide whether `OS` belongs in safe defaults for Luau embeddings.
3. [ ] Consider `camino` for resolver-facing path types because Luau module labels, diagnostics,
   and config paths are UTF-8 text; keep public APIs accepting `impl AsRef<Path>` where callers
   touch the OS.
4. [ ] Consider `tokio-util::sync::CancellationToken` as the public cancellation primitive, with a
   private bridge to the native analyzer token, instead of exposing a bespoke token type only usable
   by `ruau`.
5. [ ] Do not add small-string crates such as `smol_str` unless profiling shows `ModuleId`,
   diagnostic labels, or resolver snapshots are allocation hot spots.
6. [ ] Add an optional `diagnostics` feature only if it gives concrete value: either
   `miette::Diagnostic` conversions for application reporting or `codespan-reporting` helpers for
   examples and command-line tools. Do not make a reporting crate part of the default embedding API.
7. [ ] Remove or justify public re-exports of external crate types such as root-level `BString`;
   prefer callers importing ecosystem types directly unless the type is essential to `ruau`.

2. Stage Two: Unify Module Resolution

The project now has both `resolver` and `luau::Require`/`FsRequirer`. Choose one public model and
make runtime loading, checked loading, and analysis use it.

1. [ ] Make `crates/ruau/src/resolver.rs` the canonical public module-resolution API.
2. [ ] Hide or delete `ruau::Require`, `ruau::FsRequirer`, and `Luau::create_require_function`
   unless there is a concrete embedder use case that cannot be represented by `ModuleResolver`.
3. [ ] Replace `ResolverSnapshot::resolve`'s `string_requires` scanner with Luau-native require
   discovery, either through `RequireTracer`, an AST-backed shim, or an owned parser wrapper.
4. [ ] Port full require-by-string behavior into `FilesystemResolver`, including aliases,
   ancestry-based config merging, `.config.luau`, `.luaurc`, ambiguity handling, and virtual file
   system hooks.
5. [ ] Decide `.lua` file policy explicitly. Luau's tooling still recognizes `.lua`, so either keep
   `.lua` for require parity or make `.luau`-only a documented crate-level breaking choice.
6. [ ] Relax `ModuleResolver: Send + Sync + 'static` if snapshots remain eagerly resolved and no
   resolver is retained by native code. Tokio current-thread embedders should be able to use local
   VFS state.
7. [ ] Add an async resolver path for Tokio applications, for example `AsyncModuleResolver` and
   `ResolverSnapshot::resolve_async`, so filesystem or service-backed modules do not require
   blocking reads on the runtime thread.
8. [ ] Ensure `Luau::checked_load`, ordinary runtime `require`, and `Checker::check_snapshot` all
   consume the same `ResolverSnapshot` semantics and cache keys.

3. Stage Three: Tokio-First Embedding

Make the common Tokio embedding path obvious without pretending VM handles are freely shareable.

1. [ ] Remove unnecessary `Send` bounds from `HostApi` installers and function closures in
   `crates/ruau/src/host.rs`; `Luau::create_function` and `create_async_function` are local VM APIs
   and should allow local captures.
2. [ ] Split `HostApi` into an immutable definition bundle and repeatable runtime installer, or make
   `install(&self, &Luau)` possible, so one host description can be installed into multiple VMs and
   loaded into multiple checkers.
3. [ ] Add a small Tokio embedding helper or example that shows `LocalSet` when users spawn local
   Luau tasks, while keeping the core VM type `Send + !Sync`.
4. [ ] Add `checked_load_resolved_async` or an equivalent async flow that resolves modules with a
   Tokio-aware resolver before running the synchronous native checker.
5. [ ] Unify analyzer cancellation, runtime interrupt cancellation, and Tokio timeouts in docs and
   examples. If `tokio-util` is adopted, expose conversions rather than two unrelated tokens.
6. [ ] Review `Function::call`, `Chunk::exec`, `Chunk::eval`, and `Thread::into_async` docs for
   current-thread assumptions and make non-`Send` future behavior explicit.
7. [ ] Add an end-to-end test that uses host definitions, async host functions, async resolution,
   checked loading, cancellation, and execution under `tokio::task::LocalSet`.

4. Stage Four: Add Luau-Native Capabilities

The crate is no longer a general Lua facade. Lean into Luau's current runtime, analyzer, and type
system features.

1. [ ] Expand `Buffer` beyond raw byte copying: add checked typed reads/writes and bit-level
   helpers that mirror Luau's `buffer.readbits` and `buffer.writebits` behavior.
2. [ ] Expand `Vector` conversions and ergonomics: `From<[f32; 3]>`, `From<Vector> for [f32; 3]`,
   serde round-trips, and compiler constants that assume the built-in `vector` library.
3. [ ] Reassess hidden `Compiler::set_vector_ctor` and `set_vector_type`; the built-in vector
   library now makes custom constructor/type shims less central.
4. [ ] Add a first-class native CodeGen surface around support detection, per-module compilation,
   compile results, memory limits, and `--!native` behavior instead of only
   `Luau::enable_jit(bool)`.
5. [ ] Add analyzer options for language mode, lint configuration, lint-as-error, type-as-error,
   globals, and config-file loading. Map these to Luau's `.config.luau` and `.luaurc` semantics.
6. [ ] Extend schema extraction to understand Luau generics, type packs, exported aliases,
   user-defined type functions, and `types` library constructs where that matters to host APIs.
7. [ ] Make diagnostics richer: diagnostic code/name when available, lint category, full module
   path, related spans if native Analysis exposes them, and source text integration for reporters.
8. [ ] Add coverage and heap/memory-category examples that are Luau-specific rather than inherited
   Lua debug API examples.
9. [ ] Audit `ruau-sys` bindings after each Luau source bump for new C APIs around require,
   CodeGen, buffers, userdata tags, coverage, and analyzer options.

5. Stage Five: Remove Lua-Era Holdovers

Remove public concepts that mainly exist because the crate used to track broader Lua APIs or `mlua`
style.

1. [ ] Rename or remove the public `ruau::luau` module. Once the whole crate is Luau-only, a
   "Luau-specific extensions" namespace is redundant.
2. [ ] Remove root-level `String = LuauString`; it conflicts with `std::string::String` and is a
   holdover from Lua-binding APIs where `String` meant VM string.
3. [ ] Redesign `prelude.rs`. A `ruau::prelude::*` should not need every type renamed with a
   `Luau` prefix; export the core traits, `Luau`, `Value`, `Table`, `Function`, and `Result`
   directly, with aliases only where they avoid real collisions.
4. [ ] Remove docs that link ordinary runtime concepts to Lua 5.2 or Lua 5.4 manuals when Luau docs
   or vendored Luau headers are the real authority.
5. [ ] Reconsider `unsafe_new` and `unsafe_new_with`. If the issue is loading debug/unsafe
   libraries, expose that directly; do not keep language about Lua C modules unless it is still
   true.
6. [ ] Reconsider `register_module`/`unload_module` with `@` names and lowercasing. Prefer the
   canonical resolver/cache model over package-style module injection.
7. [ ] Make binary bytecode loading an explicit unsafe or opt-in API if malformed bytecode can crash
   the interpreter, instead of exposing `ChunkMode::Binary` as an ordinary safe mode.
8. [ ] Remove stale compatibility labels in examples and tests such as "Lua" comments for Luau-only
   functionality, except when deliberately testing `.lua` filename compatibility.
9. [ ] Keep `anyhow` support optional, but do not let application error-reporting patterns leak into
   core public error types.

6. Stage Six: API Polish

Apply Rust API conventions now that compatibility with the old shape is no longer the goal.

1. [ ] Rename consuming builder methods on `Compiler` from `set_*` to fluent names such as
   `optimization_level`, `debug_level`, `coverage_level`, `mutable_globals`, and
   `disabled_builtins`.
2. [ ] Replace numeric `u8` compiler option levels with small enums when the valid values are fixed
   and invalid values are rejected by Luau.
3. [ ] Prefer specific constructors over public option structs for states that can be invalid, but
   keep struct literals where all fields are simple data.
4. [ ] Review root re-exports in `crates/ruau/src/lib.rs` and remove duplicate paths where a module
   is already canonical.
5. [ ] Fix rustdoc broken links in `crates/ruau/src/analyzer.rs`; `cargo doc -p ruau --no-deps
   --all-features` currently reports unresolved links for `Checker`, `Diagnostic`,
   `Checker::check`, `Checker::check_path`, and `CheckOptions::virtual_modules`.
6. [ ] Add crate-level rustdoc sections for checked loading, resolver snapshots, host definitions,
   Tokio local execution, and Luau-only feature choices.
7. [ ] Re-run an API-surface review with `ruskel` after the cleanup and ensure every public item has
   exactly one intended import path.

7. Stage Seven: Validation And Release Gate

Treat this as a breaking modernization pass with strong public-surface verification.

1. [ ] Run `cargo fmt --all`.
2. [ ] Run `cargo clippy --all --all-targets --all-features --tests --examples`.
3. [ ] Run `cargo test -p ruau --all-features --no-run` before broad test runs to catch API breakage
   quickly.
4. [ ] Run `cargo xtask test`.
5. [ ] Run `cargo doc --workspace --all-features --no-deps` and require zero rustdoc warnings.
6. [ ] Add focused tests for resolver/config parity, async resolver behavior, host API reuse,
   cancellation bridging, buffer bit operations, native CodeGen controls, and bytecode safety
   policy.
7. [ ] Update `README.md`, guided tour, and examples to stop presenting the crate as a Lua-family
   binding with Luau support; present it as a Luau embedding toolkit.
8. [ ] Publish release notes that group changes by "ecosystem dependencies", "Tokio API",
   "Luau-native features", and "removed Lua-era compatibility".
