# Luau-Only Structure Cleanup

The project now has one runtime target: Luau. The remaining structural cleanup is
mostly about making the crate layout and public API match that fact: a compact
high-level embedding facade, a raw `ruau-sys` binding layer, and no public
compatibility shape left over from generic Lua/LuaJIT support.

## Implementation Notes

Implemented structure:

- Root facade keeps ordinary embedding types: `Luau`, `Result`, `Error`, `Value`, `Table`,
  `Function`, `Thread`, `AnyUserData`, `UserData`, conversion traits, `Chunk`, `Compiler`,
  `StdLib`, `Vector`, `Buffer`, `HostApi`, `LuauString`, async thread types, `Nil`,
  `MultiValue`, `Variadic`, userdata traits, and `HeapDump`.
- Canonical public namespaces are now:
  - `analyzer` and `resolver` for checked loading and module resolution.
  - `compiler` for compiler levels and compile constants.
  - `debug` for stack/function/coverage inspection.
  - `userdata` for advanced userdata references, registries, and metatables.
  - `vm` for advanced VM controls, registry handles, app data, primitive selectors, and
    numeric/light-userdata support types.
  - `serde` for serde options.
- Internal implementation modules are private: `runtime`, `state`, and `userdata_impl`.
- No `#[path = ...]` module overrides remain; moved files now use conventional module paths.
- `ruau-sys` exposes raw Luau bindings from the crate root only. The internal `compat` module is
  private and its remaining helpers are documented as Luau C API adapter helpers.
- Validation performed:
  - `cargo fmt --all` (stable rustfmt reports the repository's nightly-only config warnings).
  - `cargo check -p ruau --tests --examples`.
  - `TRYBUILD=overwrite cargo test -p ruau --test compile -- --ignored --nocapture`.
  - `cargo doc -p ruau --no-deps --all-features`.
  - `ruskel /Users/cortesi/git/private/ruau/crates/ruau`.
  - `cargo xtask tidy`.
  - `cargo xtask test`.

1. Stage One: Public Facade Audit

Establish the intended public shape before moving modules.

1. [x] Generate a reproducible public-surface snapshot with
   `cargo doc -p ruau --no-deps --all-features` plus an
   `rg "^pub " crates/ruau/src` cross-check, and record the current public
   modules, root exports, and public items inside public modules.
2. [x] Classify each public item in `crates/ruau/src/lib.rs` as root facade,
   canonical namespace, advanced module API, or internal implementation detail.
3. [x] Keep root exports focused on ordinary embedding: `Luau`, `Result`,
   `Error`, `Value`, `Table`, `Function`, `Thread`, `AnyUserData`,
   `UserData`, conversion traits, `Chunk`, `Compiler`, `StdLib`, `Vector`,
   `Buffer`, `HostApi`, `LuauString`, `AsyncThread`, `ThreadStatus`, `Nil`,
   `MultiValue`, `Variadic`, `MetaMethod`, `UserDataMethods`, and
   `UserDataFields`.
4. [x] Move advanced or low-level items out of the root facade unless users
   naturally write them in signatures.
5. [x] Update rustdoc examples to use the intended import style.

2. Stage Two: Remove The Public `ruau::luau` Namespace

The whole crate is Luau-only, so a public `luau` module no longer distinguishes
one runtime from another.

1. [x] Split `crates/ruau/src/luau/mod.rs` into private modules with concrete
   responsibilities, such as runtime `require` plumbing, heap dumps, and
   built-in global compatibility functions.
2. [x] Keep `Luau::set_module_resolver`, `Luau::set_memory_category`, and
   `Luau::heap_dump` as inherent methods on `Luau`.
3. [x] Re-export `HeapDump` from its canonical location if it remains public.
4. [x] Make resolver helper types such as shared resolver handles and module
   caches private unless external users need to construct them directly.
5. [x] Remove or update docs that refer to `ruau::luau` as a user-facing module.

3. Stage Three: Hide State And Raw Runtime Internals

The high-level crate should not expose raw state plumbing now that there is no
alternate backend abstraction to preserve.

1. [x] Verify `state::ExtraData`, `state::RawLuau`, `state::callback_error_ext`,
   `state::extra`, and `state::util` remain `pub(crate)` or narrower.
2. [x] Decide whether `pub mod state` remains as the canonical advanced VM
   namespace or becomes private with selected safe types re-exported elsewhere.
3. [x] Assign every currently public safe state type a target home:
   `LuauOptions`, `WeakLuau`, `Registry`, `GcMode`, `GcIncParams`,
   `ThreadCallbacks`, `ThreadCreateFn`, and `ThreadCollectFn`.
4. [x] Audit public signatures for `RawLuau`, `ExtraData`, raw callback types,
   and other implementation-only types.
5. [x] Keep raw native access in `ruau-sys`, not in the high-level `ruau`
   public API.
6. [x] Add `trybuild` compile-fail tests proving external code cannot name
   internal raw state types through `ruau`.

4. Stage Four: Clarify `ruau-sys` Raw Binding Shape

Luau's C API still uses `lua_*` names, but compatibility helpers should not look
like a supported alternate Lua API.

1. [x] Audit `crates/ruau-sys/src/luau/compat.rs` for helpers still used by
   `ruau` or by the generated raw binding surface.
2. [x] Keep unavoidable `lua_*` names that mirror Luau headers.
3. [x] Stop glob-re-exporting `compat::*` from `ruau-sys` if those helpers are
   only implementation support.
4. [x] If any compatibility helpers remain public, document them as Luau C API
   adapter helpers rather than Lua 5.3 compatibility support.
5. [x] Confirm `ruau-sys` docs describe the crate as raw Luau bindings, not a
   generic Lua binding layer.

5. Stage Five: Simplify Userdata Construction Paths

Userdata should read as one Luau host-object model, not a collection of legacy
wrapper variants.

1. [x] Keep `Luau::create_userdata<T: UserData>` as the primary happy-path API.
2. [x] Treat opaque userdata as an advanced path and keep it out of root-level
   examples unless the example specifically needs dynamic registration.
3. [x] Align serializable userdata cleanup with `plans/serde.md`, so serde
   support does not require separate storage variants or constructor families.
4. [x] Review `register_userdata_type`, proxies, `AnyUserData::wrap`, and
   `AnyUserData::wrap_ser` for overlap and naming consistency.
5. [x] Update userdata tests and examples around the reduced canonical model.

6. Stage Six: Narrow Advanced Luau-Specific APIs

Some APIs are valid Luau-only capabilities but should live in precise
namespaces instead of broad root exports.

1. [x] Move `PrimitiveType` out of the root facade unless examples prove users
   commonly write it in signatures.
2. [x] Keep primitive metatable APIs public only if there is a clear embedder
   use case, and place their selector type in the same canonical namespace.
3. [x] Keep app-data guards under `types` or a dedicated app-data namespace,
   not at the root.
4. [x] If `state` remains public, keep registry handles and GC tuning structs
   under `state`; if `state` becomes private, create or choose one explicit
   advanced VM namespace for `Registry`, `GcMode`, `GcIncParams`, and related
   state configuration types before moving them.
5. [x] Keep debug inspection types under `debug`.
6. [x] Move or rename advanced APIs so their module path explains their domain.
7. [x] Update docs to separate ordinary embedding, analysis/checked loading,
   userdata, serde, and advanced VM controls.

7. Stage Seven: Documentation And Validation

Finish by proving the structure is simpler and the docs match reality.

1. [x] Update the whole `README.md` feature section so it matches `Cargo.toml`;
   remove stale `serde` and `anyhow` feature claims and rewrite example
   commands such as `--features=macros,serde`.
2. [x] Remove remaining Lua/LuaJIT alternate-backend wording from high-level
   docs, while preserving raw C API names where Luau itself uses them.
3. [x] Record before/after public API counts or public module lists in the
   implementation notes.
4. [x] Run formatting, clippy, docs, and the project-standard test command.
5. [x] Do a final `rg` pass for stale public paths and compatibility language.
