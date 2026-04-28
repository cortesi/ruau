# Luau-Only Deferred Simplifications

These are follow-up cleanups found while removing non-Luau runtime support. They are useful
next steps, but not required for this pass to compile and test with Luau only.

1. Stage One: Rename Legacy Lua Wording

The public API still uses `Lua` as the core type name, which may be kept for compatibility, but
docs and comments can be more explicit about Luau.

1. [ ] Audit module-level docs for generic "Lua runtime" language that should say Luau.
2. [ ] Decide whether crate naming and exported type names remain compatibility aliases or get a
   larger rename in a separate breaking-change pass.
3. [ ] Rename internal `mlua_*` macros, `__mlua_*` registry keys, and old diagnostic text once
   public crate renaming is settled.
4. [ ] Retire or rewrite the upstream `docs/release_notes` files so they no longer describe the
   pre-rename `mlua` project as current documentation.

2. Stage Two: Tighten Tending Lints

The standard workspace lint profile is installed, but the inherited codebase currently needs
scoped exceptions for private documentation, split module file layout, and public APIs that
intentionally take owned handle types.

1. [ ] Add useful private/internal docs in `crates/ruau/src` and remove the crate-level
   `clippy::missing_docs_in_private_items` exception.
2. [ ] Decide whether `state.rs`, `types.rs`, `userdata.rs`, and `luau/require.rs` should move to
   `mod.rs` files or keep the current flat module entrypoint style.
3. [ ] Revisit `clippy::needless_pass_by_value` on public handle-taking APIs once the breaking API
   shape for `ruau` is settled.

3. Stage Three: Finish Lock-Free API Cleanup

The VM-wide reentrant mutex is gone, `Lua` is now `Send + !Sync`, and default/all-feature tests
pass. These follow-ups would finish removing compatibility scaffolding left in place to keep this
pass bounded.

1. [ ] Make async support unconditional: remove the public `async` feature, promote
   `futures-util` to a required dependency, and delete async/sync duplicate API cfg branches once
   the final public surface is chosen.
2. [ ] Delete the remaining stale `send` cfg branches, the empty `tests/send.rs`, and old trybuild
   stderr files that still mention `ReentrantMutex`.
3. [ ] Rename or remove the temporary `LuaGuard` liveness guard. It is no longer a mutex guard, but
   app-data borrows, table iterators, string views, scopes, and userdata registries still use it to
   keep liveness checks close to borrowed VM data.
4. [ ] Collapse `MaybeSend` and `MaybeSync` after the stale `send` branches are gone.
5. [ ] Replace the removed async HTTP/TCP server examples with examples designed for `Lua: !Sync`,
   probably using `tokio::task::LocalSet`, single-owner request handling, or per-connection Lua
   states rather than `tokio::spawn` with non-`Send` Lua handles.
