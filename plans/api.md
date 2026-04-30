# Ruau Public API Follow-Up

This is a concrete post-trim cleanup plan for the `ruau` crate public API. It assumes
[`plans/trim.md`](trim.md) has already landed:

- `ruau::vm` is gone.
- Raw VM internals such as `RawLuau`, `ExtraData`, `ValueRef`, and `StackCtx` are no longer
  public API.
- `Value` is `#[non_exhaustive]`.
- `Value::Other` carries `OpaqueValue`.
- The existing userdata split is deliberate: common authoring traits are at the crate root, and
  advanced borrow/registry/metatable types live under `ruau::userdata`.

There are no external backwards-compatibility constraints. Optimize for a clear pre-1.0 API, not
for preserving old spellings.

## Design Rules

1. Keep one canonical public spelling for each concept.
2. Keep high-frequency embedder types at the crate root.
3. Keep implementation details private even when hidden trait hooks use them internally.
4. Do not add replacement namespaces for removed internals.
5. Prefer hard removals over aliases when the remaining spelling is obvious inside this project.
6. Prefer explicit domain modules only when the domain is real, such as `debug` and `userdata`.

## Stage One: Collapse Duplicate Paths

These changes are mechanical and should land first.

### 1. Re-export `ObjectLike` at the crate root and make `traits` private

`ObjectLike` is the only public trait in `ruau::traits` that is not already exported at the crate
root. That makes trait imports inconsistent.

Change the root export to:

```rust
pub use crate::traits::{
    FromLuau, FromLuauMulti, IntoLuau, IntoLuauMulti, ObjectLike,
};
```

Then change `pub mod traits;` to `mod traits;`. Internal code can continue to use
`crate::traits::*`, but users get one documented path:

```rust
use ruau::{FromLuau, IntoLuau, ObjectLike};
```

Required updates:

- Replace internal examples/tests importing through `ruau::traits`.
- Verify `ObjectLike` renders at the crate root in rustdoc.
- Verify `StackCtx` does not render in public rustdoc.

### 2. Remove single-value conversion wrappers on `Luau`

The conversion traits are the canonical API. Remove these wrappers:

```rust
Luau::pack(...)
Luau::unpack(...)
Luau::convert(...)
```

Use the root-exported traits directly:

```rust
let value = input.into_luau(&lua)?;
let output = T::from_luau(value, &lua)?;
```

Also remove the multi-value wrappers:

```rust
Luau::pack_multi(...)
Luau::unpack_multi(...)
```

Use the multi-value traits directly:

```rust
let values = input.into_luau_multi(&lua)?;
let output = T::from_luau_multi(values, &lua)?;
```

Reasoning:

- The wrappers create two spellings for every conversion.
- `pack` / `unpack` are especially misleading for single values.
- The traits are already part of the main root facade, so the explicit calls are not obscure.

Required updates:

- Replace `lua.unpack(...)` in `serde/ser.rs` with `FromLuau::from_luau(...)`.
- Replace conversion wrapper uses in tests/examples with trait calls.
- Add focused tests around trait-based single and multi conversion if current wrapper tests were
  the only coverage.

### 3. Remove `Luau::null()`

`Luau::null()` and `Value::NULL` produce the same sentinel. The sentinel is not VM-derived, so the
VM method is noise.

Keep:

```rust
Value::NULL
```

Remove:

```rust
Luau::null()
```

Required updates:

- Replace `lua.null()` with `Value::NULL`.
- Update serde docs so null is described as the tagged sentinel.

### 4. Remove `LuauWorkerError::from_luau`

`LuauWorkerError` already implements `From<ruau::Error>`. Remove the duplicate inherent method:

```rust
LuauWorkerError::from_luau(error)
```

Keep:

```rust
LuauWorkerError::from(error)
let worker_error: LuauWorkerError = error.into();
```

## Stage Two: Make Set-Like APIs Actually Set-Like

This stage removes builder aliases where the type is really a set or list.

### 5. Replace `StdLib` builder methods with bitwise operators

`StdLib` is a set of flags. The clear API is constants plus operators, not fluent per-flag
methods.

Add:

```rust
impl std::ops::BitOr for StdLib { ... }
impl std::ops::BitOrAssign for StdLib { ... }
```

Remove all constructor and per-library builder methods:

```rust
StdLib::empty()
StdLib::all_safe()
StdLib::all()
StdLib::coroutine()
StdLib::table()
StdLib::os()
StdLib::string()
StdLib::utf8()
StdLib::bit32()
StdLib::math()
StdLib::buffer()
StdLib::vector()
StdLib::integer()
StdLib::debug()
```

Keep the constants and query method:

```rust
StdLib::NONE
StdLib::COROUTINE
StdLib::TABLE
StdLib::OS
StdLib::STRING
StdLib::UTF8
StdLib::BIT32
StdLib::MATH
StdLib::BUFFER
StdLib::VECTOR
StdLib::INTEGER
StdLib::DEBUG
StdLib::ALL_SAFE
StdLib::ALL
StdLib::contains(...)
```

New usage:

```rust
let libs = StdLib::MATH | StdLib::STRING | StdLib::TABLE;
let mut libs = StdLib::NONE;
libs |= StdLib::BUFFER;
```

Do not expose `StdLibBits`. `StdLib` remains the public newtype boundary.

Required updates:

- Replace docs examples using chained builders.
- Replace tests for builder methods with operator tests.
- Replace internal `StdLib::all_safe()` / `StdLib::all()` uses with constants.

### 6. Remove singular compiler list adders

The `Compiler` list-style builder methods currently have two spellings: singular appenders and
plural setters. Keep the field-style plural setters and remove the singular adders.

Remove:

```rust
Compiler::add_mutable_global(...)
Compiler::add_userdata_type(...)
Compiler::add_disabled_builtin(...)
```

Keep:

```rust
Compiler::mutable_globals(...)
Compiler::userdata_types(...)
Compiler::disabled_builtins(...)
```

For a single item, use a one-element array:

```rust
Compiler::new().mutable_globals(["game"])
```

Reasoning:

- Existing builder methods such as `optimization_level`, `debug_level`, and `coverage_level` are
  setters.
- The plural methods already match that style by replacing the whole list.
- One-element arrays are explicit and remove the append-vs-replace distinction.

Keep `add_library_constant` and `add_vector_constant`; they do not have plural setter duplicates
and represent map insertion rather than replacing one of the compiler option lists.

Required updates:

- Replace all uses of the singular adders.
- Keep tests that prove the plural methods populate compiler options correctly.

## Stage Three: Rename Ambiguous Adapters

These are small but visible naming improvements.

### 7. Rename `ExternalResult::into_luau_err` to `into_luau_result`

`ExternalError::into_luau_err` converts an error into `ruau::Error`.
`ExternalResult::into_luau_err` converts a `Result<T, E>` into `ruau::Result<T>`.

The shared method name hides the difference. Keep both traits, but give the result adapter a result
name.

Keep:

```rust
ExternalError::into_luau_err(self) -> Error
```

Rename:

```rust
ExternalResult::into_luau_err(self) -> Result<T>
```

to:

```rust
ExternalResult::into_luau_result(self) -> Result<T>
```

Required updates:

- Replace examples and tests that call `.into_luau_err()?` on external `Result` values.
- Keep `.into_luau_err()` on raw external error values.
- Add one test or doctest covering the new `ExternalResult` method name.

### 8. Rename `HostApi::definition` to `add_definition`

`HostApi::definition(...)` appends one definition. `HostApi::definitions()` returns all
definitions. The names are too close.

Rename:

```rust
HostApi::definition(...)
```

to:

```rust
HostApi::add_definition(...)
```

Keep:

```rust
HostApi::definitions()
```

Required updates:

- Replace examples/tests/docs that build host APIs with a standalone definition.
- Keep `global_function`, `global_async_function`, and `namespace` unchanged.

## Stage Four: Ergonomic Corrections

These are concrete decisions, but each should be a small independent patch.

### 9. Keep `debug` as the public debugging domain

Do not promote these functions onto `Luau`:

```rust
debug::inspect_stack(...)
debug::traceback(...)
```

Keep `ruau::debug` as the canonical public path. The crate root should stay focused on ordinary
embedding, while stack inspection and traceback generation are a debugging domain.

Required updates:

- Mention `ruau::debug` from crate-level docs.
- Keep the internal `Luau` wrappers crate-private if they remain useful.

### 10. Change `Buffer::cursor` to borrow `&self`

`Buffer` is a clonable handle. Consuming it to create a cursor is needlessly awkward.

Change:

```rust
pub fn cursor(self) -> impl io::Read + io::Write + io::Seek
```

to:

```rust
pub fn cursor(&self) -> impl io::Read + io::Write + io::Seek
```

The implementation should clone the handle internally:

```rust
BufferCursor(self.clone(), 0)
```

Required updates:

- Document that cursors share the same underlying Luau buffer.
- Add a test that the original `Buffer` remains usable after creating a cursor.

### 11. Do not add `Table::is_safeenv`

Keep `Table::set_safeenv(bool)` write-only. The Luau C API binding in this repository exposes
`lua_setsafeenv`, but not a supported getter.

Required updates:

- Document on `set_safeenv` that Luau exposes this flag as write-only through the bound API.
- Do not read Luau internals to synthesize a getter.

### 12. Keep explicit default/configured method pairs

Do not replace these with `impl Into<Options>` or `()`-based overloads:

```text
Checker::check               / check_with_options
Checker::check_path          / check_path_with_options
Luau::to_value               / to_value_with
Luau::deserialize_value             / deserialize_value_with
Luau::new                    / new_with
```

The explicit pairs are verbose but clear in rustdoc and examples. Generic option conversion would
hide the configured path instead of clarifying it.

### 13. Keep `wrap` constructors and document the adapter pattern

Keep:

```rust
LuauString::wrap(...)
AnyUserData::wrap(...)
Chunk::wrap(...)
```

Do not replace them with blanket `IntoLuau` impls. The `wrap` methods are explicit deferred
conversion adapters, and blanket impls are more likely to create coherence or ambiguity problems.

Required updates:

- Cross-link the three `wrap` methods.
- Use consistent wording: "deferred conversion adapter".
- Explain when `wrap` is better than creating the value eagerly with `Luau::create_*`.

### 14. Document `OpaqueValue` production cases

`OpaqueValue` exists because `Value` is non-exhaustive and Luau can produce values that this crate
does not model directly.

Required updates:

- Add rustdoc explaining when users might see `Value::Other`.
- Make clear that users can compare, move, store, and pass opaque values back to Luau, but cannot
  inspect their internals through the public API.

## Implementation Order

Use small commits in this order:

1. Root-export `ObjectLike`; make `traits` private; update imports/docs.
2. Remove `Luau` conversion wrappers; update code to use conversion traits.
3. Remove `Luau::null()` and `LuauWorkerError::from_luau`.
4. Replace `StdLib` builders with `BitOr` / `BitOrAssign`.
5. Remove singular `Compiler` list adders.
6. Rename the `ExternalResult` method to `into_luau_result` and `HostApi::definition` to
   `add_definition`.
7. Change `Buffer::cursor(&self)` and document shared cursor backing.
8. Apply docs-only clarifications for `debug`, `set_safeenv`, `wrap`, and `OpaqueValue`.

## Validation

After each code stage, run the smallest relevant test first. Before the final commit, run:

```bash
cargo fmt --all
cargo check -p ruau --tests --examples --features macros
cargo nextest run -p ruau --features macros
cargo doc -p ruau --no-deps --features macros
git diff --check
```

Also inspect rustdoc or use a compile-fail test to confirm:

- `ruau::traits` is not public.
- `StackCtx` is not public.
- `ObjectLike` is available from `ruau::ObjectLike`.
