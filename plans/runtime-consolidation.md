# Runtime Consolidation Plan

Implemented in this pass.

The repository now builds around a single Luau runtime shape:

- `Lua` is movable but not shareable: `Send + !Sync`.
- Async support is unconditional and Tokio-backed.
- Luau execution entrypoints such as `Function::call`, `Chunk::exec`, `Chunk::eval`, and object
  calls return futures by default.
- Runtime feature variance for `send`, `async`, `error-send`, `serialize`, `userdata-wrappers`,
  `luau-vector4`, and `luau-jit` has been removed.
- `parking_lot::ReentrantMutex` and the old VM-wide lock model have been removed.
- `LuaGuard` was renamed to `LuaLiveGuard`, and stale lock-oriented comments were cleaned up.
- Tests, examples, benches, and doctests have been updated for the async default API.

Validation completed:

1. [x] `cargo fmt --all`
2. [x] `cargo test`
3. [x] `cargo xtask test`
4. [x] `cargo xtask tidy`
5. [x] Targeted `rg` checks for removed feature names and stale lock vocabulary

Deferred follow-ups discovered during implementation are tracked in `plans/luau.md`.
