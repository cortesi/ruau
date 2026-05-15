# ruau — Simplification Follow-Up

## Current State

The post-Luau-specialization simplification pass is complete. The original nine
stages landed as one commit per stage:

- `cd0406c refactor(ruau): replace XRc alias`
- `b3c3e1a refactor(sys): remove thin compat shims`
- `31a784c refactor(ruau): unify protected error helpers`
- `4d09afd refactor(ruau): clarify native integer boundary`
- `acd5deb refactor(ruau): tighten userdata type registry`
- `2c6c4ae refactor(ruau): remove gc mode wrapper`
- `e4a7bce refactor(sys): flatten vendored luau build`
- `ff45173 refactor(ruau): unify borrowed string data`
- `4dc1d97 refactor(ruau): collapse host function builders`

The last full validation after Stage 9 was green:

- `cargo xtask tidy`
- `cargo xtask test` with 388 tests passed plus doctests

Post-stage measurement: **30,327** Rust src LOC. Unsafe audit remained at
`ruau` **93** `unsafe fn`, `ruau-sys` **61** `unsafe fn`, and **260** unsafe
blocks in `ruau`.

## Forward-Facing Next Steps

There are no obvious remaining simplification stages from the original review
that meet the bar for immediate implementation. The next useful work is
consumer-facing cleanup around the public API changes:

- Add migration notes or release notes for removed/renamed public APIs:
  `HostNamespace`, `HostApi::{global_function,global_async_function,namespace,try_namespace}`,
  and `GcMode` / `gc_set_mode`.
- Check downstream in-tree or sibling consumers before merge/release, especially
  code that used the old host builder callback style.
- If another simplification pass is desired, start from a fresh source review
  rather than continuing this checklist; the remaining candidates from the
  original sweep were either rejected or depend on concrete new pain points.

## Carry-Forward Decisions

Do not reopen these without new evidence:

- Keep `Value::Integer`. Luau has a native integer type, but primitive Rust
  integer conversions intentionally preserve Luau `number` semantics.
- Keep `lua_geti`, `lua_seti`, and `lua_len`; they implement Luau-compatible
  numeric indexing and length behavior around metamethods.
- Keep `lua_rawgeti` / `lua_rawseti` unless a later cleanup has a stronger
  reason than spreading `c_int` narrowing at call sites.
- Keep the userdata borrow cell instead of replacing it with `RefCell`; borrow
  conflicts must return `Error::UserDataBorrowError`, not panic across FFI.
- Keep the auxiliary `ref_thread`; it is a deliberate handle-storage mechanism,
  not leftover version scaffolding.
- Keep the `Luau` / `RawLuau` split unless a concrete API or safety issue justifies
  a larger redesign.
- Keep `StdLib` bitflags, `ErrorContext`, `LuauInterruptPolicy`, `SafetyError`,
  and `short_type_name`; these are live user-facing surfaces or behavior.

## Release Gate

Before merging or releasing this branch, rerun:

- `cargo xtask tidy`
- `cargo xtask test`
