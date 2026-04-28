# Runtime Consolidation Plan

This plan spins out the old Stage Three from `plans/luau.md` and folds in the runtime decisions
from `plans/next.md` and `plans/next-2.md`.

The goal is to finish the inherited `mlua` runtime collapse now that the VM-wide reentrant mutex is
gone. The clean target for this pass is:

- `Lua` remains movable across threads but not shareable: `Send + !Sync`.
- Luau execution becomes async-only, while ordinary value access remains synchronous.
- Async is a built-in Tokio-shaped runtime contract, not an optional compatibility mode.
- Dead sharing, locking, and feature-variance scaffolding is removed before analyzer work starts.

This plan does not start the repo-owned Luau source build, `ruau-analyze`, tagged userdata, or
atom-based `__namecall`. Those are important, but they depend on this cleanup being stable first.

1. Stage One: Remove Dead Sharing Modes

Delete the leftover configuration paths that no longer correspond to supported behavior.

1. [ ] Remove every `cfg(feature = "send")` and `cfg(not(feature = "send"))` branch, keeping the
   single-owner `Lua: Send + !Sync` path.
2. [ ] Collapse `MaybeSend` and `MaybeSync` into direct bounds or remove them entirely where no
   bound remains useful.
3. [ ] Remove the stale `error-send` feature and settle on one error object shape.
4. [ ] Remove the deprecated `serialize` feature alias if it still exists after the feature audit.
5. [ ] Delete empty or obsolete send tests and update trybuild stderr files that still mention the
   old `ReentrantMutex` implementation.

2. Stage Two: Normalize Runtime Features

Reduce runtime build variance to the useful feature set before the async API rewrite.

1. [ ] Remove `userdata-wrappers` if no current design needs it; this is the likely path to
   deleting the remaining `parking_lot` dependency.
2. [ ] Remove `luau-vector4`. Keep `Vector` three-wide; add a distinct type later if four-wide
   vectors become a real requirement.
3. [ ] Make `luau-jit` a runtime setting instead of a build feature if the current `luau0-src`
   build can support always-linked CodeGen cleanly.
4. [ ] If always-linked JIT is blocked by the external source crate, record that blocker here and
   defer the JIT feature removal to the repo-owned Luau source build.
5. [ ] Leave `serde`, `macros`, and `anyhow` as ordinary integration features.

3. Stage Three: Make Async Unconditional

Stop testing and maintaining a synchronous-only embedding mode.

1. [ ] Remove the public `async` feature from `crates/ruau/Cargo.toml`.
2. [ ] Promote async support dependencies from optional to required for `ruau`.
3. [ ] Delete `cfg(feature = "async")` and docsrs async feature gates throughout source, tests,
   examples, and benches.
4. [ ] Keep both `create_function` and `create_async_function`; synchronous Rust callbacks are
   still useful for cheap host functions.
5. [ ] Ensure `cargo test` runs the async coverage by default without feature flags.

4. Stage Four: Commit To Tokio

Make the async implementation match the project decision instead of preserving runtime-agnostic
plumbing inherited from `mlua`.

1. [ ] Add Tokio as a normal dependency with the smallest feature set needed by the runtime.
2. [ ] Replace generic waker signalling with Tokio primitives where this removes custom state or
   polling code.
3. [ ] Define cancellation in Tokio terms: dropping an in-flight async thread interrupts the Luau
   coroutine and surfaces as `Error::Cancelled`.
4. [ ] Update async examples around `tokio::task::LocalSet`, single-owner request handling, or
   per-connection `Lua` states rather than `tokio::spawn` with shared Lua handles.
5. [ ] Avoid adding a cross-thread actor handle in this pass; keep that as a future public API
   design once the single-owner runtime is smaller.

5. Stage Five: Move Luau Execution To Async-Only API

Apply the public API decision from `plans/next-2.md`: sync data access remains, but executing Luau
code always returns a future.

1. [ ] Rename async execution methods to the primary names: `call_async` to `call`,
   `exec_async` to `exec`, `eval_async` to `eval`, and equivalent chunk/thread methods.
2. [ ] Remove or make private the old synchronous Luau execution entrypoints.
3. [ ] Update tests, examples, benches, docs, and compile-fail expectations to use `.await`.
4. [ ] Keep synchronous table, string, userdata, conversion, and app-data access APIs.
5. [ ] Audit async callbacks so borrowed Luau data is not accidentally held across `.await` in
   public examples or tests.

6. Stage Six: Modernize Async Signatures

Use the newer Rust features that are now available to this fork.

1. [ ] Evaluate `create_async_function` with stable `AsyncFn` bounds, but do not take owned `Lua`
   in callbacks unless a distinct affine context handle is introduced.
2. [ ] Convert user-implementable async traits such as `Require` to RPITIT where it removes boxed
   futures without making the API harder to read.
3. [ ] Audit remaining `BoxFuture` and `Pin<Box<dyn Future>>` boundaries after the execution API is
   async-only.
4. [ ] Defer `thiserror` and `tracing` unless they naturally simplify code touched in this pass;
   they are useful cleanup, not prerequisites for runtime consolidation.

7. Stage Seven: Rename Liveness Guards And Lock Vocabulary

Finish removing the conceptual residue from the deleted VM mutex.

1. [ ] Rename or replace `LuaGuard` with a name that describes liveness, not locking.
2. [ ] Update app-data borrows, table iterators, string views, scopes, and userdata registries to
   use the new liveness guard shape.
3. [ ] Remove comments and diagnostics that describe VM access as protected by a reentrant mutex.
4. [ ] Search for `lock`, `guard`, `send`, `sync`, and `ReentrantMutex`; keep only terms that still
   describe real local synchronization or public Rust auto-trait behavior.

8. Stage Eight: Validate And Trim Follow-Ups

Leave the repository compiling cleanly with the new default shape.

1. [ ] Run `cargo fmt --all`.
2. [ ] Run `cargo test`.
3. [ ] Run `cargo xtask test` if available and not duplicative of the default test sweep.
4. [ ] Run `cargo xtask tidy` or the repository lint equivalent.
5. [ ] Run targeted `rg` checks for removed feature names and lock vocabulary.
6. [ ] Remove completed runtime consolidation items from `plans/luau.md`; add any deferred
   simplifications discovered during implementation to the most relevant plan file.
