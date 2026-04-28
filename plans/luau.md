# Luau-Only Deferred Simplifications

These are follow-up cleanups found while removing non-Luau runtime support. They are useful
next steps, but not required for this pass to compile and test with Luau only.

1. Stage One: Collapse Always-Luau Cfgs

Many source files still contain `#[cfg(feature = "luau")]` and `#[cfg(not(feature =
"luau"))]` branches that are now statically known.

1. [ ] Remove unreachable non-Luau branches from `src/debug.rs`, `src/state.rs`,
   `src/state/raw.rs`, `src/function.rs`, and `src/userdata.rs`.
2. [ ] Remove Luau-only cfg attributes from public Luau APIs once the crate requires Luau.
3. [ ] Re-run rustdoc after cfg cleanup to catch stale `doc(cfg(...))` references.
4. [ ] Consider removing the `luau` feature flag entirely and making Luau unconditional, so
   `--no-default-features` is either unsupported by manifest design or has a tiny cfg-gated surface.
5. [ ] Simplify remaining tests that still contain old Lua/LuaJIT cfg branches, especially
   `tests/tests.rs`, `tests/userdata.rs`, `tests/memory.rs`, and `tests/thread.rs`.

2. Stage Two: Rename Legacy Lua Wording

The public API still uses `Lua` as the core type name, which may be kept for compatibility, but
docs and comments can be more explicit about Luau.

1. [ ] Audit module-level docs for generic "Lua runtime" language that should say Luau.
2. [ ] Decide whether crate naming and exported type names remain compatibility aliases or get a
   larger rename in a separate breaking-change pass.
