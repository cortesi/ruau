# API Cleanup

This is a clean-break plan for making `ruau` more idiomatic, powerful, and
simple after integrating analysis. The goal is to reduce duplicate public paths,
hide invalid states, tighten analyzer ergonomics, and keep host/resolver APIs
small enough to hold in one mental model.

## Decisions

- Treat all changes here as breaking changes. Do not add deprecations, aliases,
  or compatibility shims.
- Prefer one canonical public path for each concept. Root re-exports are fine
  for core facade types, but avoid also exposing the same type through a public
  module unless the module is the canonical namespace.
- Keep `ruau-sys` and native shim details out of the high-level API.
- Do not expose unsafe/raw escape-hatch endpoints from `ruau`. If raw native
  access is needed for internal work, keep it in `ruau-sys` or private modules.
- Optimize the public API for the common case: embedding Luau in a Tokio app
  with typed Rust callbacks, host definitions, checked loading, and async
  execution.
- Prefer constructors and methods that make invalid states impossible over
  public fields that users can assemble incorrectly.
- Prefer focused explicit APIs over clever generation. Add builders only where
  options are genuinely hard to read as struct literals.

## Stage 1: Canonicalize Public Export Paths

Goal: remove duplicate paths and make the public API easier to discover.

1. [x] Decide which namespaces are canonical:
   - root facade for common runtime types (`Luau`, `Function`, `Table`, `Value`)
   - `analyzer` for checking types and functions
   - `resolver` for module graph/resolution contracts
2. [x] Make `host` private and expose `HostApi` only from one path, preferably
   `ruau::analyzer::HostApi` or `ruau::HostApi`, not both.
3. [x] Keep `module_schema` private and expose schema extraction only from the
   analyzer namespace if it remains part of the public analyzer API.
4. [x] Review root `pub use` items in `crates/ruau/src/lib.rs` and remove
   duplicate public paths introduced by the analyzer work.
5. [x] Remove root exports of `ffi`, `lua_State`, and `lua_CFunction` from the
   high-level `ruau` API. Keep raw native access in `ruau-sys`, not as part of
   the embedding API.
6. [x] Audit public modules for other unsafe/raw implementation endpoints and
   make them private unless they are necessary for ordinary Tokio embedding.
7. [x] Add rustdoc examples that show the intended import style for checking,
   checked loading, and host definitions.

## Stage 2: Tighten Analyzer Results And Errors

Goal: make check results and failures obvious and actionable.

1. [x] Rename `analyzer::Error` to `AnalysisError`, or split it into
   `CheckError` and `CheckedLoadError` if checked loading needs different
   variants from plain checking.
2. [x] Change `CheckResult::is_ok()` so it returns `false` for errors,
   timeouts, and cancellation. If the current meaning is still useful, expose
   it as `has_errors()` only.
3. [x] Make `CheckResult::errors()` and `CheckResult::warnings()` return
   iterators instead of allocating `Vec<&Diagnostic>`.
4. [x] Add module identity to `Diagnostic`, either directly as `module:
   ModuleId` or through a `SourceLocation { module, span }` field.
5. [x] Replace raw line/column fields on `Diagnostic` with a `SourceSpan` field
   if doing so reduces duplication without making call sites noisier.
6. [x] Remove `CheckerPolicy` from the public API unless embedders need runtime
   introspection. Keep fixed policy covered by tests instead.
7. [x] Rename native-construction error text and docs away from old wording like
   "native library load"; the checker is now statically linked.

## Stage 3: Make Resolver Snapshots Valid By Construction

Goal: prevent users from building inconsistent module graphs manually.

1. [x] Make `ResolverSnapshot` fields private.
2. [x] Expose read-only accessors:
   - `root() -> &ModuleId`
   - `root_source() -> Option<&ModuleSource>`
   - `modules() -> impl Iterator<Item = &ModuleSource>`
   - `dependency(requester, specifier) -> Option<&ModuleSource>`
3. [x] Collapse `ResolvedModule` and `ModuleSource` into one public type unless
   a real user-facing distinction remains.
4. [x] Add a small `ResolverSnapshotBuilder` only if tests or embedders need to
   construct snapshots without a resolver.
5. [x] Make `ModuleId` implement `Display`, `AsRef<str>`, and `From<PathBuf>` or
   a dedicated path constructor if filesystem IDs are expected.
6. [x] Rename `FilesystemResolver::new(root: impl Into<PathBuf>)` to accept
   `impl AsRef<Path>` if the resolver does not need ownership from the caller.
7. [x] Add resolver tests for relative dependencies, duplicate module names,
   path-backed diagnostics, and invalid manual graph construction being
   impossible.

## Stage 4: Simplify Checked Loading

Goal: make checked runtime loading concise and harder to misuse.

1. [x] Change `Luau::checked_load` to consume `ResolverSnapshot` or otherwise
   return a chunk that does not borrow the caller's snapshot.
2. [x] Consider a convenience method:
   `Luau::checked_load_resolved(&mut Checker, &impl ModuleResolver, root)`.
   Keep it only if it does not hide important resolver errors.
3. [x] Ensure checked loading returns analyzer diagnostics before any VM
   mutation, including host definition and dependency failures.
4. [x] Add a checked-loading error type that distinguishes:
   - analysis diagnostics
   - missing root
   - resolver failure
   - runtime environment setup failure
5. [x] Add tests proving failed root and failed dependency checks leave globals
   and module cache state unchanged.
6. [x] Add a Tokio-first checked-loading example that creates a VM, registers
   async Rust callbacks, checks a module graph, and executes it without any raw
   native API usage.

## Stage 5: Design The Tokio Embedding Surface

Goal: make the common Tokio embedding path obvious and complete.

1. [x] Add or refine a high-level embedding example that starts from
   `Luau::new()`, registers host functions, checks code, and awaits execution.
2. [x] Ensure async callback registration names and trait bounds read naturally
   for Tokio users. Hide any executor or VM plumbing that embedders do not need
   to reason about.
3. [x] Review cancellation APIs from the embedding perspective:
   - analyzer cancellation through `CancellationToken`
   - runtime cancellation through VM interrupt support
   - clear docs explaining when each applies
4. [x] Ensure the checked-loading API composes with Tokio task structure without
   requiring `Send + Sync` on VM handles that cannot safely support it.
5. [x] Add one end-to-end Tokio integration test that exercises host callbacks,
   checked loading, module resolution, and async execution.

## Stage 6: Redesign Host Definitions As A Small Builder

Goal: keep runtime registration and analyzer definitions paired without making
host APIs pretend to be complete code generation.

1. [x] Decide whether the canonical type is `HostApi`, `HostDefinitions`, or
   `HostRegistry`. Pick the name that matches the actual scope after this stage.
2. [x] Require definition strings to be full `.d.luau` declarations, or provide
   separate methods for full declarations and function signatures. Do not keep
   one method that guesses both formats.
3. [x] Add `definitions(...)` or `definition_file(...)` for host definitions
   that are not tied to one runtime global.
4. [x] Add explicit methods for common host shapes only when they are complete:
   - global function
   - global value/table
   - userdata registration
5. [x] Ensure `install` and `add_definitions_to` can be called in either order
   without surprising ownership constraints. If necessary, split the runtime
   installer from the immutable definition bundle.
6. [x] Add examples showing host definitions used with both `Checker::check`
   and `Luau::checked_load`.

## Stage 7: Documentation And Examples

Goal: make the clean-break API self-explanatory from rustdoc and examples.

1. [x] Update crate-level docs in `crates/ruau/src/lib.rs` with an "Analysis
   and checked loading" section focused on Tokio embedding.
2. [x] Add rustdoc examples for:
   - checking one source string
   - checking a resolver snapshot
   - checked loading a graph
   - host definitions paired with runtime globals
   - async Rust callbacks in Tokio
3. [x] Update examples under `crates/ruau/examples` or add a focused analyzer
   example if examples are currently runtime-only.
4. [x] Audit docs for stale `Lua` wording and old native-library analyzer
   packaging language.
5. [x] Ensure all public newtypes and builder types explain ownership and
   thread-safety plainly.

## Stage 8: Validation

Goal: prove the API cleanup did not weaken behavior.

1. [x] Run `cargo xtask tidy`.
2. [x] Run `cargo xtask test`.
3. [x] Run `cargo doc --workspace --all-features --no-deps` and inspect the
   public analyzer/resolver/host pages for duplicate or confusing paths.
4. [x] Use `ruskel /Users/cortesi/git/private/ruau/crates/ruau --all-features`
   to verify the final public API shape.
5. [x] Commit the API cleanup as one cohesive breaking-change commit after
   review and approval.
