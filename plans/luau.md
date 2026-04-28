# Luau-Only Deferred Simplifications

These are follow-up cleanups found while removing non-Luau runtime support. They are useful
next steps, but not required for this pass to compile and test with Luau only.

1. Stage One: Collapse Always-Luau Cfgs

Many source files still contain `#[cfg(feature = "luau")]` and `#[cfg(not(feature =
"luau"))]` branches that are now statically known.

1. [ ] Remove unreachable non-Luau branches from `crates/ruau/src/debug.rs`,
   `crates/ruau/src/state.rs`, `crates/ruau/src/state/raw.rs`,
   `crates/ruau/src/function.rs`, and `crates/ruau/src/userdata.rs`.
2. [ ] Remove Luau-only cfg attributes from public Luau APIs once the crate requires Luau.
3. [ ] Re-run rustdoc after cfg cleanup to catch stale `doc(cfg(...))` references.
4. [ ] Consider removing the `luau` feature flag entirely and making Luau unconditional, so
   `--no-default-features` is either unsupported by manifest design or has a tiny cfg-gated surface.
5. [ ] Simplify remaining tests that still contain old Lua/LuaJIT cfg branches, especially
   `crates/ruau/tests/tests.rs`, `crates/ruau/tests/userdata.rs`,
   `crates/ruau/tests/memory.rs`, and `crates/ruau/tests/thread.rs`.

2. Stage Two: Rename Legacy Lua Wording

The public API still uses `Lua` as the core type name, which may be kept for compatibility, but
docs and comments can be more explicit about Luau.

1. [ ] Audit module-level docs for generic "Lua runtime" language that should say Luau.
2. [ ] Decide whether crate naming and exported type names remain compatibility aliases or get a
   larger rename in a separate breaking-change pass.
3. [ ] Rename internal `mlua_*` macros, `__mlua_*` registry keys, and old diagnostic text once
   public crate renaming is settled.
4. [ ] Retire or rewrite the upstream `docs/release_notes` files so they no longer describe the
   pre-rename `mlua` project as current documentation.

3. Stage Three: Tighten Tending Lints

The standard workspace lint profile is installed, but the inherited codebase currently needs
scoped exceptions for private documentation, split module file layout, and public APIs that
intentionally take owned handle types.

1. [ ] Add useful private/internal docs in `crates/ruau/src` and remove the crate-level
   `clippy::missing_docs_in_private_items` exception.
2. [ ] Decide whether `state.rs`, `types.rs`, `userdata.rs`, and `luau/require.rs` should move to
   `mod.rs` files or keep the current flat module entrypoint style.
3. [ ] Revisit `clippy::needless_pass_by_value` on public handle-taking APIs once the breaking API
   shape for `ruau` is settled.
