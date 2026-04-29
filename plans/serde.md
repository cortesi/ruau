# Serde And Userdata Cleanup

Serde is now always available in `ruau`, so the public API should stop carrying
feature-era split paths. The goal is to make plain data flow through serde
conversion APIs, keep userdata focused on Rust object identity and methods, and
remove serializable-userdata storage complexity without weakening embedding
semantics.

1. Stage One: Documentation And Surface Audit

Confirm the current public contract before changing behavior.

1. [ ] Update `README.md` and examples to stop describing `serde` as an optional
   feature.
2. [ ] Audit public serde-facing exports in `crates/ruau/src/lib.rs`, especially
   `SerializeOptions`, `DeserializeOptions`, and `SerializableValue`, and decide
   which names remain part of the canonical API.
3. [ ] Audit serializable-userdata entry points:
   `Luau::create_ser_userdata`, `Luau::create_ser_any_userdata`, and
   `AnyUserData::wrap_ser`.
4. [ ] Update examples so plain structs and JSON-like data prefer
   `Luau::to_value` and `Luau::from_value` instead of serializable userdata.

2. Stage Two: Clarify The Data/Object Boundary

Make the API guidance explicit: serde is for value-shaped data, userdata is for
host objects with identity, borrowing, methods, mutation, and metatables.

1. [ ] Add rustdoc guidance to `serde` docs explaining when to use
   `to_value`/`from_value` versus userdata.
2. [ ] Add rustdoc guidance to `UserData` explaining that serde does not replace
   userdata when object identity, methods, mutation, scoped borrows, destructors,
   or proxies are required.
3. [ ] Review `examples/serde.rs` and `examples/userdata.rs` so they demonstrate
   this split without overlapping too much.
4. [ ] Add or adjust tests that show serde round-tripping plain data does not
   require userdata.

3. Stage Three: Replace Serializable Userdata Storage

Remove the current `UserDataStorage::Serializable` path, which erases `T` behind
`dyn erased_serde::Serialize` and casts back to `T` for borrow/take operations.

1. [ ] Design a registration-time serde hook for userdata, for example a
   `UserDataRegistry<T>` opt-in that is available when `T: serde::Serialize`.
2. [ ] Store userdata as concrete `T` regardless of whether serde support is
   enabled.
3. [ ] Move serde dispatch for userdata from storage variants to type metadata
   or registry callbacks.
4. [ ] Preserve existing behavior for `Value::UserData(ud)` serialization when
   the userdata type opted into serde support.
5. [ ] Add tests covering borrow, borrow_mut, take, destroy, and serialization
   for a serde-enabled userdata type.

4. Stage Four: Retire `*_ser_*` Constructors

Collapse public constructors once the new serde hook can represent the same
behavior without changing storage layout.

1. [ ] Replace internal uses of `create_ser_userdata`,
   `create_ser_any_userdata`, and `AnyUserData::wrap_ser` with the new model.
2. [ ] Decide whether this release removes the `*_ser_*` APIs outright or keeps
   temporary deprecated shims.
3. [ ] If shims remain, implement them in terms of ordinary userdata creation
   plus serde registration rather than a separate storage variant.
4. [ ] Remove tests that exist only to validate the old constructor split, or
   rewrite them around the new canonical path.

5. Stage Five: Simplify Serde Options And Wrappers

Make the always-on serde API smaller without losing useful table-serialization
controls.

1. [ ] Review whether root-exporting `SerializableValue` is still necessary, or
   whether option-specific methods should live behind clearer `Value`/`Table`
   entry points.
2. [ ] Keep `SerializeOptions` and `DeserializeOptions` if they remain the
   clearest way to control nulls, array metatables, sorted keys, mixed tables,
   and unsupported types.
3. [ ] Ensure option names no longer read as compatibility shims for an optional
   feature.
4. [ ] Update serde tests to cover the final public option surface.

6. Stage Six: Unsafe And Dependency Cleanup

Use the serde cleanup to reduce unsafe code and remove dependencies that only
exist for the old storage strategy.

1. [ ] Remove the `UserDataVariant::Serializable` branch and the raw pointer cast
   in `crates/ruau/src/userdata/cell.rs`.
2. [ ] Remove `erased-serde` if no longer needed after userdata serialization
   moves to registered callbacks.
3. [ ] Re-run the unsafe audit for userdata modules and document the remaining
   unsafe sites that are still required for Luau stack/userdata handling.
4. [ ] Run the focused userdata and serde tests, then the project-standard full
   test command.
