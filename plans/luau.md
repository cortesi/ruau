# Luau-Only API Simplification Plan

This is a clean-break plan for simplifying `ruau` now that the project targets Luau
only, with no alternate Lua or LuaJIT backend. The goal is to remove generic
Lua-framework leftovers, make Luau-specific capabilities feel first-class, and reduce
the public API to the concepts embedders actually need.

## Decisions

- Treat these changes as breaking changes. Do not add compatibility aliases unless
  a staged migration is explicitly requested later.
- Optimize the high-level `ruau` crate for Luau embedding, not for dialect-neutral
  Lua abstraction.
- Keep analysis, checked loading, host declarations, and resolver support in the
  core API; they are part of the Luau embedding story, not optional extras.
- Keep raw C API naming in `ruau-sys` where it mirrors Luau headers, but keep that
  shape out of the high-level API.
- Prefer explicit Luau concepts (`buffer`, `vector`, checked loading, safeenv,
  readonly tables) over generic compatibility knobs.
- Prefer one canonical public path for each concept.
- Hide escape hatches unless they are needed by normal embedding code.

## Stage 1: Remove Remaining Lua-Compatibility Framing

Goal: make docs, examples, and resolver behavior match a Luau-only crate.

1. [ ] Replace remaining public docs that describe the crate in Lua-family or
   alternate-backend terms with Luau-only wording.
2. [ ] Update `README.md` feature flag documentation so it matches the current
   `Cargo.toml` feature set and does not mention removed features.
3. [ ] Change `FilesystemResolver` in `crates/ruau/src/resolver.rs` to resolve
   `.luau` files only by default.
4. [ ] Remove implicit `.lua` lookup from the default resolver path.
5. [ ] Add an explicit extension override, such as
   `FilesystemResolver::with_extensions(["luau", "lua"])`, for projects that
   intentionally keep Luau source in `.lua` files.
6. [ ] Treat extension override order as precedence: with
   `["luau", "lua"]`, `foo.luau` wins over `foo.lua`; do not preserve the old
   ambiguity error for configured fallback extensions.
7. [ ] Add resolver tests for `.luau` lookup, missing modules, disabled implicit
   `.lua` lookup, explicit extension override precedence, and `init.luau`
   directory resolution.

## Stage 2: Simplify The Runtime Facade

Goal: make the root API read like one compact Luau embedding facade.

1. [ ] Review root exports in `crates/ruau/src/lib.rs` and keep only common
   embedding types at the root.
2. [ ] Move advanced inspection and bookkeeping types out of the root facade:
   `Debug*`, `CoverageInfo`, `FunctionInfo`, `HeapDump`, `Registry`, `WeakLuau`,
   `Scope`, iterator structs, app-data guards, raw string borrowing helpers,
   GC tuning structs, and low-level registry handles.
3. [ ] Keep root exports for the core runtime types: `Luau`, `Result`, `Error`,
   `Value`, `Table`, `Function`, `Thread`, `AnyUserData`, `UserData`,
   conversion traits, `Chunk`, `Compiler`, `StdLib`, `Vector`, and `Buffer`.
4. [ ] Also keep root exports for types users naturally write in signatures or
   common registrations: `MultiValue`, `Variadic`, `Nil`, `MetaMethod`,
   `UserDataMethods`, `UserDataFields`, `AsChunk`, `LuauString`,
   `AsyncThread`, `ThreadStatus`, `HostApi`, and `HostNamespace`.
5. [ ] Move configuration and advanced helper types to their owning modules:
   compiler levels under `chunk`, serde options under `serde`, external-error
   helpers under `error`, userdata references and registries under `userdata`,
   and VM state or registry-key helpers under `types` or `state`.
6. [ ] Confirm the remaining public import style in docs and examples.
7. [ ] Run `ruskel` on `crates/ruau` and record the before/after public item
   count in the PR or implementation notes.

## Stage 3: Make Luau Source Loading Direct

Goal: remove source-mode abstraction that mostly exists for generic loaders.

1. [ ] Replace public `ChunkMode` use with explicit loading methods:
   `Luau::load(source)` for text and
   `unsafe Luau::load_bytecode(bytes) -> Result<Function>` for trusted Luau
   bytecode.
2. [ ] Remove or make private `Chunk::text_mode` and `Chunk::binary_mode`.
3. [ ] Keep `Chunk` as the text-source builder only; `compiler`, expression
   `eval`, and source-name conveniences remain text-only behavior.
4. [ ] Document that bytecode callers can configure environment after loading
   with `Function::set_environment` when needed.
5. [ ] Keep bytecode safety documentation on the explicit bytecode API.
6. [ ] Update `AsChunk` so normal users do not need to understand source modes.
7. [ ] Update tests and examples that currently set chunk modes.

## Stage 4: Narrow Primitive And Numeric Abstractions

Goal: stop exporting generic Lua-runtime abstractions from the high-level API.

1. [ ] Remove root promotion of `Integer` and `Number`, or move them to a
   low-level namespace if they are still useful for exact FFI matching.
2. [ ] Prefer normal Rust numeric types in public docs and examples.
3. [ ] Replace `type_metatable<T: LuauType>` and `set_type_metatable<T:
   LuauType>` with a concrete Luau primitive selector if this capability remains
   public.
4. [ ] Replace the private-bound `LuauType` generic on public methods with the
   concrete primitive selector from the previous item, then remove
   `#[allow(private_bounds)]` from those methods.
5. [ ] Audit `Value` numeric helpers and remove redundant casts that normal
   `TryFrom` or pattern matching can express clearly.

## Stage 5: Collapse Userdata Construction Variants

Goal: reduce userdata creation to one common path plus explicit advanced wrappers.

1. [ ] Keep `Luau::create_userdata<T: UserData>` as the primary public API.
2. [ ] Replace `create_ser_userdata` and `create_ser_any_userdata` with an
   explicit wrapper or option if serializable userdata must remain supported.
3. [ ] Rename `Luau::create_any_userdata` to `Luau::create_opaque_userdata`,
   document it in an advanced opaque-userdata section, and remove it from
   happy-path rustdoc examples.
4. [ ] Keep `register_userdata_type` only as part of the opaque-userdata flow
   paired with `create_opaque_userdata`.
5. [ ] Update `AnyUserData::wrap` and `AnyUserData::wrap_ser` to follow the new
   construction model or remove them.
6. [ ] Update userdata tests so they cover the reduced public API without relying
   on removed variants.

## Stage 6: Trim Userdata Registration Methods

Goal: keep the common userdata registration API readable.

1. [ ] Split rare registration operations from `UserDataMethods` and
   `UserDataFields` into extension traits or concrete `UserDataRegistry<T>`
   methods.
2. [ ] Keep the ordinary method and field operations easy to discover:
   immutable method, mutable method, async method, async mutable method,
   function, mutable function, field getter, and field setter.
3. [ ] Keep metatable registration explicit rather than hidden behind the word
   "metamethod": document the receiver-taking `add_meta_method` family, the
   raw-argument `add_meta_function` family, and meta field helpers as separate
   concepts.
4. [ ] Reconsider the `method` versus `function` distinction and document the
   one remaining mental model clearly.
5. [ ] Keep `*_once` consuming methods only if examples show a compelling common
   use case.
6. [ ] Update rustdoc examples to show the smallest viable userdata definition.

## Stage 7: Simplify The Core Analysis Surface

Goal: keep checked loading core while hiding analysis internals that are not user
workflows.

1. [ ] Keep `analyzer`, `resolver`, checked loading, `HostApi`, and host
   declarations available in the default public API.
2. [ ] Keep runtime resolver contracts public and default:
   `ModuleResolver`, `ModuleId`, `ModuleSource`, `ModuleResolveError`,
   `FilesystemResolver`, `InMemoryResolver`, `ResolverSnapshot`, and
   `Luau::set_module_resolver`.
3. [ ] Keep checked-loading entry points public: `Luau::checked_load`,
   `Luau::checked_load_resolved`, `Checker`, diagnostics, and checker options.
4. [ ] Remove public analyzer re-exports of `ModuleSchema`, `ClassSchema`,
   `ModuleRoot`, `ModuleSchemaError`, `NamespaceSchema`, and
   `extract_module_schema`; demote schema extraction to `pub(crate)` unless a
   concrete user story justifies exposing declaration-shape introspection.
5. [ ] Keep the crate-level doctest focused on the core checked-loading path,
   but make it concise enough that the first public example is not dominated by
   setup.
6. [ ] Ensure examples show both direct runtime loading and checked loading as
   core workflows.

## Stage 8: Clean Up `ruau-sys` Export Paths

Goal: keep the FFI crate broad but not unnecessarily duplicated.

1. [ ] Make flat root exports the canonical public FFI import style.
2. [ ] Remove public duplicate module export paths between
   `crates/ruau-sys/src/lib.rs` and `crates/ruau-sys/src/luau/mod.rs`.
3. [ ] Keep `lua_*` and `LUA_*` names in `ruau-sys`; they mirror Luau's C API
   and should not be renamed in the raw binding layer.
4. [ ] Fix ruskel/rustfmt skeleton issues around raw identifiers such as
   `r#ref` so the FFI public surface can be inspected reliably.
5. [ ] Update internal imports in `ruau` to the chosen FFI path.

## Stage 9: Validation

Goal: prove the simplification keeps Luau behavior intact.

1. [ ] Run `cargo fmt --all`.
2. [ ] Run the smallest relevant tests after each stage.
3. [ ] Run `cargo xtask test` after the full API simplification.
4. [ ] Run `cargo xtask tidy`.
5. [ ] Run `cargo doc --workspace --all-features --no-deps`.
6. [ ] Run `ruskel /Users/cortesi/git/private/ruau/crates/ruau --all-features`
   and inspect the final public API shape.
7. [ ] Review docs for stale Lua/LuaJIT compatibility language.
