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

3. Stage Three: Finish Runtime Async Cleanup

Runtime consolidation is implemented, but a few narrower cleanups were intentionally deferred to
keep this pass focused on compiling Luau-only defaults.

1. [ ] Replace the public doc-hidden `call_sync`, `exec_sync`, and `eval_sync` escape hatches with
   crate-private execution helpers, while preserving a safe internal path for synchronous Rust
   callbacks and scope tests.
2. [ ] Decide whether `Thread::resume` remains the one synchronous Luau execution exception or
   moves to an async primary API alongside `Thread::into_async`.
3. [ ] Replace the remaining custom async-thread waker/recycle plumbing with Tokio primitives where
   that actually removes code, and define dropped in-flight Luau work in terms of a dedicated
   cancellation error.
4. [ ] Rework async examples around `tokio::task::LocalSet`, single-owner request handling, or
   per-connection `Lua` states instead of only making them compile with async calls.
5. [ ] Revisit the `anyhow` integration so `anyhow::Error -> ruau::Error` can preserve useful
   context without requiring `ruau::Error: Send + Sync`; the current conversion intentionally
   flattens to `RuntimeError`.
6. [ ] Audit remaining `BoxFuture` and `Pin<Box<dyn Future>>` boundaries now that async execution
   is unconditional.
