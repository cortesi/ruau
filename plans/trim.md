# Public API Trim-Back

This plan trims the high-level `ruau` API toward a smaller, intentional embedding facade.
It explicitly retains `EntrypointSchema` and `LightUserData`; the focus is on removing
duplicate paths, hidden internals, and sharp low-level hooks from the stable public surface.

## Ground Rules

- **Pre-1.0 policy.** `ruau` is `0.12.0-rc.1`. All removals in this plan are hard removals
  in a single release. No `#[deprecated]` aliases, no transitional `pub use` re-exports.
- **Single canonical path per public type.** Every retained public type has exactly one
  fully-qualified path the docs link to.
- **Internal modules stay private.** Moving an item out of a re-export module means
  promoting it to a root `pub use` from a private inner module, not exposing the inner
  module.
- **Tests track decisions.** Compile-fail tests are the authority on what cannot be
  named from outside the crate; doctest examples are the authority on what is
  publicly callable.

## Retained Decisions

- Keep `analyzer::EntrypointSchema`, `analyzer::EntrypointParam`, and
  `analyzer::extract_entrypoint_schema`.
- Keep `LightUserData` public as the representation for Luau lightuserdata values and
  `Value::NULL`.
- Keep the ordinary embedding facade: `Luau`, `Value`, `Table`, `Function`, `Thread`,
  `AnyUserData`, `UserData`, conversion traits, `Chunk`, `Compiler`, `StdLib`, `Buffer`,
  `Vector`, `HostApi`, `LuauWorker`, `analyzer`, and `resolver`.
- Keep `ThreadCallbacks`, `set_thread_callbacks`, `type_metatable`, `set_type_metatable`,
  `create_proxy`, `AnyUserData::destroy`, and `UserDataMetatable`/`UserDataMetatablePairs`
  — all have active test or example callers and are documented embedder workflows.

## Stage Zero: Inventory

Build a single source of truth for the current surface before touching anything. This
prevents "make X canonical" tasks that don't actually move any items.

1. [x] Generate `cargo doc -p ruau --no-deps --features macros` and capture every public
   item with its current canonical path into `plans/trim-inventory.md`.
2. [x] In that inventory, mark each item as one of: keep-as-is, re-home (canonical path
   change), make-private, or remove.
3. [x] Cross-check the inventory against the `pub mod` and root `pub use` list in
   `crates/ruau/src/lib.rs:130-230` so nothing slips through.

## Stage One: Freeze Canonical Export Paths

The `ruau::vm` module is currently the only path to `LightUserData`, `Integer`, `Number`,
`PrimitiveType`, `RegistryKey`, `WeakLuau`, `LuauOptions`, `GcMode`, `GcIncParams`,
`Registry`, `VmState`, `Scope`, `AppData*`, `ThreadCallbacks`, `ThreadCreateFn`, and
`ThreadCollectFn`. There is no `ruau::compiler` module today, so prior references to it
are dropped from the plan.

1. [x] Promote the following to root `pub use` in `crates/ruau/src/lib.rs`:
   `LightUserData`, `Integer`, `Number`, `PrimitiveType`, `RegistryKey`, `WeakLuau`,
   `LuauOptions`, `GcMode`, `GcIncParams`, `Registry`, `ThreadCallbacks`,
   `ThreadCreateFn`, `ThreadCollectFn`, `VmState`.
2. [x] Delete `crates/ruau/src/vm.rs` and the `pub mod vm;` declaration. There is no
   replacement re-export module — root is the canonical path.
3. [x] Decide `Scope` and `AppData`/`AppDataRef`/`AppDataRefMut`:
   - **Scope**: keep public at root; it appears in compile-fail test diagnostics
     (`tests/compile/scope_*.stderr`), confirming it is part of the user-visible surface.
   - **AppData\***: keep public at root; this is the supported host-side state mechanism.
4. [x] Update `tests/value.rs:214,276,277` and `tests/luau.rs:89` to import from the
   crate root instead of `ruau::vm::*`.
5. [x] Regenerate compile-fail expected output (`tests/compile/scope_*.stderr`) so it
   reads `ruau::Scope<...>` rather than `ruau::vm::Scope<...>`.
6. [x] Audit `ruau::userdata` re-exports in `crates/ruau/src/userdata.rs`. Currently it
   exports `UserDataMetatable`, `UserDataMetatablePairs`, `UserDataOwned`, `UserDataRef`,
   `UserDataRefMut`, `UserDataRegistry`. None overlap with the root `pub use`
   (`AnyUserData`, `MetaMethod`, `UserData`, `UserDataFields`, `UserDataMethods`).
   Keep this module as the advanced-API namespace; do not promote its contents to root.

## Stage Two: Hide Raw VM Internals

The high-level crate must not expose raw state machinery; `ruau-sys` is the raw
binding layer. Several items currently leak through `state::mod.rs:22-24` and
`types/mod.rs`.

1. [x] In `crates/ruau/src/state/mod.rs:22-24`, change `pub use extra::ExtraData;`,
   `pub use raw::RawLuau;`, and `pub use util::callback_error_ext;` to `pub(crate)`.
2. [x] In `crates/ruau/src/state/mod.rs`, change `LuauLiveGuard` from `pub` to
   `pub(crate)`.
3. [x] In `crates/ruau/src/types/mod.rs`, change `pub use value_ref::{ValueRef,
   ValueRefIndex};` to `pub(crate)`.
4. [x] In `crates/ruau/src/types/mod.rs`, change the following type aliases and structs
   to `pub(crate)`: `Callback`, `CallbackPtr`, `ScopedCallback`, `Upvalue`,
   `CallbackUpvalue`, `AsyncCallback`, `AsyncCallbackUpvalue`, `AsyncPollUpvalue`,
   `InterruptCallback`, `ThreadCreationCallback`, `ThreadCollectionCallback`,
   `DestructedUserdata`.
5. [x] In `crates/ruau/src/traits.rs:22`, change `StackCtx` to `pub(crate)`. Audit the
   `#[doc(hidden)]` stack-level methods on `IntoLuau`/`FromLuau`/`IntoLuauMulti`/
   `FromLuauMulti` so they no longer reference any name that just became private.
6. [x] Add a compile-fail test under `tests/compile/raw_internals_hidden.rs` that tries
   to import each of: `ruau::RawLuau`, `ruau::ExtraData`, `ruau::ValueRef`,
   `ruau::Callback`, `ruau::StackCtx`. The test must fail to compile.

## Stage Three: Prune Userdata Internals

Keep the typed userdata model. Make the following implementation-detail items
crate-private; each is a separate task because each touches different code:

1. [x] `RawUserDataRegistry` → `pub(crate)`.
2. [x] `UserDataStorage` → `pub(crate)`.
3. [x] `UserDataProxy` → `pub(crate)`.
4. [x] `TypeIdHints` → `pub(crate)`.
5. [x] `borrow_userdata_scoped` and `borrow_userdata_scoped_mut` (free functions) →
   `pub(crate)`.
6. [x] `collect_userdata` (free function) → `pub(crate)`.
7. [x] `init_userdata_metatable` (free function) → `pub(crate)`.
8. [x] Confirm `AnyUserData::destroy` (`userdata_impl/mod.rs:650`) stays public —
   referenced by `tests/scope.rs:577,589` and `tests/userdata.rs:327,357,366,990`.
9. [x] Confirm `Luau::create_proxy` (`state/mod.rs:1190`) stays public — referenced by
   `tests/userdata.rs:719` and the inline doctest at `state/mod.rs:1183`.
10. [x] Confirm `UserDataMetatable` and `UserDataMetatablePairs` stay public — they are
    the only public surface for inspecting a userdata's metatable, exposed via
    `AnyUserData::metatable()`.
11. [x] Add a compile-fail test confirming `ruau::userdata::RawUserDataRegistry` and
    `ruau::userdata::UserDataStorage` cannot be named.

## Stage Four: Narrow VM Lifecycle Hooks

Retain safe VM configuration; trim only what does not have an active embedder caller.

1. [x] Keep `LuauOptions`, `GcMode`, `GcIncParams`, `Registry`, `RegistryKey`,
   `WeakLuau`, `VmState` — all are used by documented workflows or tests.
2. [x] Keep `ThreadCallbacks`, `ThreadCreateFn`, `ThreadCollectFn`, and
   `Luau::set_thread_callbacks` — actively exercised in `tests/luau.rs:289,319,336,350`.
   Do not redesign the callback payload: `on_collect` necessarily fires after the Luau
   thread is collected, so a `LightUserData` identity token is the only correct shape.
   Document this on `ThreadCollectFn`.
3. [x] Remove `Luau::set_fflag` (`state/mod.rs:820`) from the public surface. It is
   already `#[doc(hidden)]` and has no caller outside the crate. Change the function
   to `pub(crate)` and keep it for crate-internal test usage; if a test outside the
   crate needs it, add a `#[cfg(test)]`-gated public alias rather than a permanent
   public method.
4. [x] Keep `Luau::type_metatable` and `Luau::set_type_metatable`
   (`state/mod.rs:1203,1238`) — `tests/value.rs:214` and `tests/luau.rs:89` use them
   to install Buffer and Vector metatables, which is a supported embedder pattern.

## Stage Five: Make `Value` Forward-Compatible

`Value` changes need to land before the serde stage because serde adapters match
on `Value` variants.

1. [x] Add `#[non_exhaustive]` to `pub enum Value` in `crates/ruau/src/value.rs:36-63`.
   This lets future Luau additions land without a major bump.
2. [x] Replace `Value::Other(#[doc(hidden)] ValueRef)` with
   `Value::Other(OpaqueValue)`, where `OpaqueValue` is a new public wrapper struct
   in `value.rs` whose only public surface is `Debug`, `type_name(&self) -> &'static
   str`, and round-trip through `Luau` (push/pop). The inner `ValueRef` becomes a
   private field, so external matches cannot extract it.
3. [x] Update the internal call sites that match on `Value::Other(vref)` —
   `value.rs:62,89,196,234,587,612,755` and `table.rs` serde paths — to work via the
   new wrapper's crate-private accessors.
4. [x] Add tests in `tests/value.rs` covering: `Value::NULL` round-trip, lightuserdata
   round-trip, and an unknown-typed value flowing through `OpaqueValue` and back to
   Luau without loss.
5. [x] Update any pattern-matching examples in rustdoc that destructure `Value` to
   include a `_` arm, demonstrating the `#[non_exhaustive]` shape.

## Stage Six: Clean Serde Public Surface

Serde exposes options and `Luau` entry points; everything else is internal.

1. [x] Confirm `Luau::to_value`, `Luau::to_value_with`, `Luau::from_value`,
   `Luau::from_value_with`, `serde::SerializeOptions`, and `serde::DeserializeOptions`
   stay public — these are the documented serde entry points.
2. [x] Confirm `serde::Serializer` and `serde::Deserializer` stay private. They already
   live in `pub(crate) mod de;` and `pub(crate) mod ser;` (`serde/mod.rs:233,234`); no
   change needed beyond an inventory check.
3. [x] Audit the helper structs `MapPairs`, `RecursionGuard`, `SerializeSeq`,
   `SerializeTupleVariant`, `SerializeMap`, `SerializeStruct`, `SerializeStructVariant`
   inside `serde/ser.rs` and `serde/de.rs`. Every one of them must be `pub(crate)` (or
   private).
4. [x] Make `Value::to_serializable` (`value.rs:515`) `pub(crate)`. Its only callers are
   `value.rs:650` (the `Serialize` impl on `Value`) and `table.rs:1149,1174,1197,1198`.
   External callers route through `Luau::to_value` or the `Serialize` impl on `Value`.
5. [x] Make `SerializableValue` (`value.rs:640`) `pub(crate)`. It is the customization
   carrier behind `to_serializable` and is not part of the serde entry-point story.
6. [x] Update the serde module-level docstring (`serde/mod.rs:1-11`) so `NULL` is
   described as a tagged sentinel, not as a general-purpose `LightUserData` pointer.

## Stage Seven: Validation And Migration

1. [x] Run `cargo fmt --all`.
2. [x] Run `cargo check -p ruau --tests --examples --features macros`.
3. [x] Run `cargo nextest run -p ruau --features macros` (or `cargo test` if nextest is
   unavailable) to confirm the moved tests still compile and pass.
4. [x] Run `trybuild` for the new compile-fail tests added in Stages Two and Three.
5. [x] Run `cargo doc -p ruau --no-deps --features macros` and diff the public module
   list against the Stage Zero inventory. Every removed entry should be gone; no new
   surprise entries should appear.
6. [x] Add a `MIGRATING.md` (or section in CHANGELOG) listing:
   - Removed paths: `ruau::vm::*`.
   - Re-homed paths: `LightUserData`, `Integer`, `Number`, `PrimitiveType`,
     `RegistryKey`, `WeakLuau`, `LuauOptions`, `GcMode`, `GcIncParams`, `Registry`,
     `ThreadCallbacks`, `ThreadCreateFn`, `ThreadCollectFn`, `VmState`, `Scope`,
     `AppData*` — now at crate root.
   - Made private: `RawLuau`, `ExtraData`, `ValueRef`, `ValueRefIndex`, `StackCtx`,
     all `Callback`/`Upvalue` aliases, `RawUserDataRegistry`, `UserDataStorage`,
     `UserDataProxy`, `TypeIdHints`, `to_serializable`, `SerializableValue`,
     `set_fflag`.
   - Shape changes: `Value` is now `#[non_exhaustive]`; `Value::Other` carries an opaque
     `OpaqueValue` wrapper rather than a raw `ValueRef`.
   - Retained: `EntrypointSchema`, `LightUserData`, `ThreadCallbacks`,
     `type_metatable`/`set_type_metatable`, `create_proxy`, `AnyUserData::destroy`,
     `UserDataMetatable`/`UserDataMetatablePairs`.
