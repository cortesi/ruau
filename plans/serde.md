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
2. [ ] Audit public serde-facing exports in `crates/ruau/src/lib.rs` and the
   public `serde` and `value` modules. Treat `SerializeOptions` and
   `DeserializeOptions` as module exports from `ruau::serde`, and treat
   `SerializableValue` as `ruau::value::SerializableValue` rather than a
   crate-root export.
3. [ ] Audit serializable-userdata entry points:
   `Luau::create_serializable_userdata`,
   `Luau::create_serializable_opaque_userdata`, and
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
   this split without overlapping too much. In particular, move the current
   `examples/serde.rs` `Car` flow away from `create_serializable_userdata`
   unless the example is deliberately demonstrating userdata serialization.
4. [ ] Add or adjust tests that show serde round-tripping plain data does not
   require userdata.

3. Stage Three: Replace Serializable Userdata Storage

Remove the current `UserDataStorage::Serializable` path, which erases `T` behind
`dyn erased_serde::Serialize` and casts back to `T` for borrow/take operations.

1. [ ] Extend the existing `UserDataRegistry<T>` with an explicit serde opt-in
   for `T: serde::Serialize`; do not introduce a parallel registry type.
2. [ ] Store userdata created through the new registration-based serde path as
   concrete `T` regardless of whether serde support is enabled.
3. [ ] Move serde dispatch for userdata from storage variants to type metadata,
   registry callbacks, or metatable-owned callbacks installed by
   `UserDataRegistry<T>`.
4. [ ] Replace the current `Serialize for UserDataStorage<()>` implementation
   with a design that queries the registered serializer callback, validates the
   userdata type, borrows the concrete `T`, and forwards serialization without a
   `UserDataVariant::Serializable` storage branch.
5. [ ] Before removing the old serializer branch, reimplement
   `create_serializable_userdata`, `create_serializable_opaque_userdata`, and
   `AnyUserData::wrap_ser` as temporary shims over the new concrete-storage
   serde path, or keep a temporary `UserDataVariant::Serializable` fallback in
   `Serialize for UserDataStorage<()>` until those shims are in place. Do not
   leave live public constructors that still create `Serializable`-variant
   storage after the serializer branch stops handling that variant.
6. [ ] Replace `AnyUserData::is_serializable` and
   `UserDataStorage::is_serializable` with a registry-backed predicate for
   whether userdata has opted into serde support. Update the serialization path
   in `value.rs` and the deserialization paths in `serde/de.rs` so both use the
   new predicate instead of checking for `UserDataVariant::Serializable`.
7. [ ] Preserve existing behavior for `Value::UserData(ud)` serialization:
   serde-enabled userdata serializes as the opted-in Rust value, unsupported
   userdata respects the existing `deny_unsupported_types` option, and destroyed
   or mismatched userdata still errors correctly.
8. [ ] Preserve existing behavior for deserializing serde-enabled userdata from
   `Value::UserData(ud)`, including the current `serde/de.rs` paths that treat
   serializable userdata as a deserializable value.
9. [ ] Decide at the end of this stage whether the constructor APIs will be
   removed outright or kept as deprecated shims in Stage Four. Base the decision
   on whether the new registry hook can represent the old behavior without a
   separate storage variant.
10. [ ] Decide and document the migration path for opaque serde userdata whose
    type only satisfies `T: Serialize + 'static` and does not implement
    `UserData`. Either provide a convenience API that preserves the current
    single-call ergonomics of `create_serializable_opaque_userdata` and
    `AnyUserData::wrap_ser`, or explicitly document the new two-step
    `register_userdata_type::<T>(...)` plus `create_opaque_userdata(data)`
    workflow.
11. [ ] Add tests covering borrow, borrow_mut, take, destroy, serialization, and
   deserialization for a serde-enabled userdata type, plus unsupported-userdata
   behavior under the final option surface.

4. Stage Four: Retire Serializable Userdata Constructors

Collapse public constructors once the new serde hook can represent the same
behavior without changing storage layout.

1. [ ] Replace internal uses of `create_serializable_userdata`,
   `create_serializable_opaque_userdata`, and `AnyUserData::wrap_ser` with the
   new registration-based model where possible.
2. [ ] If Stage Three chose deprecated shims, implement them in terms of ordinary
   userdata creation plus serde registration rather than a separate storage
   variant.
3. [ ] If Stage Three chose removal, remove the serializable constructors and
   update call sites, docs, and tests to use the canonical path chosen for
   opaque `T: Serialize` userdata.
4. [ ] Remove tests that exist only to validate the old constructor split, or
   rewrite them around the new canonical path.

5. Stage Five: Simplify Serde Options And Wrappers

Make the always-on serde API smaller without losing useful table-serialization
controls.

1. [ ] Review whether `ruau::value::SerializableValue` should remain public,
   become crate-private, or gain a clearer public wrapper path through
   `Value`/`Table` entry points. Do not frame this as removing a crate-root
   export; it is not root-exported today.
2. [ ] Keep `SerializeOptions` and `DeserializeOptions` if they remain the
   clearest way to control nulls, array metatables, sorted keys, mixed tables,
   recursive tables, and unsupported types.
3. [ ] Ensure option names no longer read as compatibility shims for an optional
   feature.
4. [ ] Update serde tests to cover the final public option surface.

6. Stage Six: Unsafe And Dependency Cleanup

Use the serde cleanup to reduce unsafe code and remove dependencies that only
exist for the old storage strategy.

1. [ ] Remove the `UserDataVariant::Serializable` branch and the raw pointer cast
   in `crates/ruau/src/userdata/cell.rs` after Stage Four has moved or removed
   all callers of `create_serializable_userdata`,
   `create_serializable_opaque_userdata`, `AnyUserData::wrap_ser`, and
   `UserDataStorage::new_ser`.
2. [ ] Remove `erased-serde` if no longer needed after userdata serialization
   moves to registered callbacks. Verify whether the replacement can use the
   existing `serde-value` dependency or another concrete intermediate without
   preserving an erased `Serialize` object in storage.
3. [ ] Re-run the unsafe audit for userdata modules and document the remaining
   unsafe sites that are still required for Luau stack/userdata handling.
4. [ ] Run the focused userdata and serde tests, then the project-standard full
   test command.
