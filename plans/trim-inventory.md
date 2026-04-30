# Public API Trim Inventory

Generated after `cargo doc -p ruau --no-deps --features macros`.

## Crate Root

Keep as-is:

- Derive: `FromLuau`
- Macro: `chunk`
- Modules: `analyzer`, `debug`, `resolver`, `serde`, `traits`, `userdata`
- Structs: `AnyUserData`, `AppData`, `AppDataRef`, `AppDataRefMut`, `AsyncThread`,
  `BorrowedBytes`, `BorrowedStr`, `Buffer`, `Chunk`, `Compiler`, `Function`,
  `GcIncParams`, `HostApi`, `HostNamespace`, `LightUserData`, `Luau`, `LuauOptions`,
  `LuauString`, `LuauWorker`, `LuauWorkerBuilder`, `LuauWorkerHandle`, `MultiValue`,
  `OpaqueValue`, `Registry`, `RegistryKey`, `Scope`, `StdLib`, `Table`, `Thread`,
  `ThreadCallbacks`, `Variadic`, `Vector`, `WeakLuau`
- Enums: `CompileConstant`, `CoverageLevel`, `DebugLevel`, `Error`, `GcMode`,
  `LuauWorkerError`, `MetaMethod`, `OptimizationLevel`, `PrimitiveType`, `ThreadStatus`,
  `TypeInfoLevel`, `Value`, `VmState`
- Traits: `AsChunk`, `ErrorContext`, `ExternalError`, `ExternalResult`, `FromLuau`,
  `FromLuauMulti`, `IntoLuau`, `IntoLuauMulti`, `UserData`, `UserDataFields`,
  `UserDataMethods`
- Type aliases: `Integer`, `LuauWorkerResult`, `Number`, `Result`, `ThreadCollectFn`,
  `ThreadCreateFn`

Re-homed from `ruau::vm` to crate root:

- `AppData`, `AppDataRef`, `AppDataRefMut`, `GcIncParams`, `GcMode`, `Integer`,
  `LightUserData`, `LuauOptions`, `Number`, `PrimitiveType`, `Registry`, `RegistryKey`,
  `Scope`, `ThreadCallbacks`, `ThreadCollectFn`, `ThreadCreateFn`, `VmState`, `WeakLuau`

Removed root/module paths:

- `ruau::vm`
- `ruau::RawLuau`, `ruau::ExtraData`, `ruau::ValueRef`, `ruau::ValueRefIndex`,
  `ruau::Callback`, `ruau::StackCtx`

## Public Modules

`ruau::analyzer` keep:

- Enums: `AnalysisError`, `Severity`
- Function: `extract_entrypoint_schema`
- Structs: `CancellationToken`, `CheckOptions`, `CheckResult`, `Checker`,
  `CheckerOptions`, `Diagnostic`, `EntrypointParam`, `EntrypointSchema`, `VirtualModule`

`ruau::debug` keep:

- Functions: `inspect_stack`, `traceback`
- Structs: `CoverageInfo`, `Debug`, `DebugNames`, `DebugSource`, `DebugStack`,
  `FunctionInfo`, `HeapDump`

`ruau::resolver` keep:

- Enum: `ModuleResolveError`
- Structs: `FilesystemResolver`, `InMemoryResolver`, `ModuleId`, `ModuleSource`,
  `ResolverSnapshot`, `SourceSpan`
- Trait: `ModuleResolver`
- Type alias: `LocalResolveFuture`

`ruau::serde` keep:

- Structs: `DeserializeOptions`, `SerializeOptions`

`ruau::traits` keep:

- Traits: `FromLuau`, `FromLuauMulti`, `IntoLuau`, `IntoLuauMulti`, `ObjectLike`

`ruau::userdata` keep:

- Structs: `UserDataMetatable`, `UserDataMetatablePairs`, `UserDataOwned`,
  `UserDataRef`, `UserDataRefMut`, `UserDataRegistry`

Removed from `ruau::userdata`:

- `RawUserDataRegistry`, `UserDataProxy`, `UserDataStorage`, `TypeIdHints`,
  `borrow_userdata_scoped`, `borrow_userdata_scoped_mut`, `collect_userdata`,
  `init_userdata_metatable`

## Shape Changes

- `Value` is `#[non_exhaustive]`.
- `Value::Other` now carries `OpaqueValue`, not `ValueRef`.
- `Value::to_serializable` and `SerializableValue` are crate-private.
- `Luau::set_fflag` is crate-private.
