# Migrating Public API Trim-Back

This release removes duplicate and low-level public paths from the high-level `ruau` crate.

## Removed Paths

- `ruau::vm::*`

Use the crate root for retained VM-facing types.

## Re-Homed Paths

These items now live at the crate root:

- `AppData`, `AppDataRef`, `AppDataRefMut`
- `GcIncParams`, `GcMode`
- `Integer`, `Number`
- `LightUserData`
- `LuauOptions`
- `PrimitiveType`
- `Registry`, `RegistryKey`
- `Scope`
- `ThreadCallbacks`, `ThreadCreateFn`, `ThreadCollectFn`
- `VmState`
- `WeakLuau`

For example, replace `ruau::vm::PrimitiveType` with `ruau::PrimitiveType`.

## Made Private

The following internals are no longer public API:

- `RawLuau`, `ExtraData`
- `ValueRef`, `ValueRefIndex`
- `StackCtx`
- Callback and upvalue aliases such as `Callback`, `CallbackPtr`, `ScopedCallback`,
  `CallbackUpvalue`, `AsyncCallback`, `AsyncCallbackUpvalue`, `AsyncPollUpvalue`,
  `InterruptCallback`, `ThreadCreationCallback`, and `ThreadCollectionCallback`
- `RawUserDataRegistry`, `UserDataStorage`, `UserDataProxy`, `TypeIdHints`
- Userdata implementation helpers such as `borrow_userdata_scoped`,
  `borrow_userdata_scoped_mut`, `collect_userdata`, and `init_userdata_metatable`
- `Value::to_serializable` and `SerializableValue`
- `Luau::set_fflag`

## Shape Changes

- `Value` is now `#[non_exhaustive]`; add a wildcard arm when matching it.
- `Value::Other` now contains `OpaqueValue`, an opaque round-trip wrapper, rather than the
  raw internal `ValueRef`.

## Retained APIs

The following APIs remain public:

- `analyzer::EntrypointSchema`, `analyzer::EntrypointParam`, and
  `analyzer::extract_entrypoint_schema`
- `LightUserData` and `Value::NULL`
- `ThreadCallbacks`, `ThreadCreateFn`, `ThreadCollectFn`, and `Luau::set_thread_callbacks`
- `Luau::type_metatable` and `Luau::set_type_metatable`
- `Luau::create_proxy`
- `AnyUserData::destroy`
- `userdata::UserDataMetatable` and `userdata::UserDataMetatablePairs`
