# Reaping the Refactor: API & Internals Cleanup

Recommendations for capitalising on the recent Luau-only / Tokio-async refactor. The crate now
has zero back-compat or migration constraints, so every item below is fair game as a breaking
change.

The list is organised by leverage: high-impact items that are only possible because of the new
shape come first, followed by API-surface trims, then internal cleanup.

## High-leverage simplifications enabled by the Tokio-only refactor

### 1. Collapse `AsyncCallFuture` into plain `async fn`

`Chunk::exec`, `Chunk::eval`, and `Chunk::call` are already `async fn` after the refactor. The
remaining users of `AsyncCallFuture<R>` are:

- `Function::call` (`function.rs:203`)
- `ObjectLike::call` / `call_method` / `call_function` (`traits.rs:167-181`)
- The `ObjectLike` impls for `Table` (`table.rs:1073-1103`) and `AnyUserData`
  (`userdata/object.rs:28-56`)

`AsyncCallFuture` (`function.rs:622-638`) exists so those calls can be
`pub fn call(...) -> AsyncCallFuture<R>` and encode "fail before yielding" as
`Result<AsyncThread<R>, Error>` inside the future. With the VM `Send + !Sync` and futures
explicitly `!Send` / local, the wrapper has no remaining job:

```rust
// before
pub fn call<R>(&self, args: impl IntoLuauMulti) -> AsyncCallFuture<R>
// after
pub async fn call<R: FromLuauMulti>(&self, args: impl IntoLuauMulti) -> Result<R>
```

Drops `AsyncCallFuture` and `AsyncCallFuture::error` from the public API entirely. ~30 LOC of
struct/Future plumbing in `function.rs` plus the four call sites above. `ObjectLike`'s methods
become `async fn` in the trait too, which is fine for a `!Send` trait used only in current-thread
runtimes.

### 2. Make `Checker::check` async and use `spawn_blocking`

`Checker::check_with_options` (`analyzer.rs`) is currently a synchronous call into the native
Luau analyzer. A Tokio application that calls it directly on the runtime thread blocks the
executor for the duration of the check. Cancellation today goes through the native
`CancellationToken` (`analyzer.rs:281-334`), which signals the C side via atomic state — no
Rust-side polling bridge is needed.

The natural Tokio shape is to make `check` async and run the C call on the blocking pool, while
signaling the native token from the runtime thread when the user cancels.

**Owned FFI input package.** The current sync path builds `FfiStr<'_>`,
`ResolvedCheckOptions<'_>`, and `ResolvedVirtualModules<'_>` from borrowed `&str` and
`CheckOptions<'_>` data (`analyzer.rs:451-453`, `:694-735`, `:739-786`).
`tokio::task::spawn_blocking` needs `FnOnce() -> R + Send + 'static` for both the closure and
its return value, so:

- Source bytes, module name, and virtual modules must be copied into owned types
  (`Box<str>`, `Vec<RuauVirtualModuleOwned>`) before dispatch.
- The closure must decode the raw `RuauCheckResult` (which contains raw pointers and is not
  `Send`) into the owned `CheckResult` and free the shim allocation *inside* the blocking pool,
  so only owned data crosses the await boundary.

**Native handle ownership.** This is the subtle one. `Checker` owns `inner: RuauCheckerHandle`
(`analyzer.rs:342-346`) and frees it in `Drop` (`analyzer.rs:594-598`). A `&mut self` async
`check` only locks the checker for the *future's lifetime*. When the future is dropped — by
`timeout`, `select!`, or just user cancellation — the borrow ends, but the `spawn_blocking`
closure may still be inside `ruau_checker_check` for some milliseconds while the native token
takes effect. At that point the user's code can reuse the checker (calling `add_definitions` or
another `check`) or drop it entirely, freeing the native handle while the blocking thread is
still using it. Same problem applies to an internally created `CancellationToken`.

The fix is shared ownership of both native handles:

```rust
struct CheckerHandleInner {
    raw: ffi::RuauCheckerHandle,
    busy: AtomicBool,
}
impl Drop for CheckerHandleInner { /* frees raw */ }

pub struct Checker {
    handle: Arc<CheckerHandleInner>,
    options: CheckerOptions,
}
```

The `spawn_blocking` closure clones the `Arc`, so the C handle survives until the blocking call
returns even if the user dropped the `Checker`. `CancellationTokenInner` is already
`Arc`-wrapped, so cloning into the closure already works. The `busy` flag handles re-entry
during the post-drop tail: each `check` does `compare_exchange` to claim the slot, errors with
`Busy` if a previous call is still draining, and clears the flag in the closure's RAII guard.
In practice users rarely hit this — the native cancel returns within a tick or two — but the
flag closes the soundness hole.

```rust
pub async fn check(&mut self, source: &str, options: CheckOptions<'_>)
    -> Result<CheckResult, AnalysisError>
{
    self.handle.busy.compare_exchange(false, true, AcqRel, Acquire)
        .map_err(|_| AnalysisError::Busy)?;
    let owned = OwnedCheckInputs::from_borrowed(source, &options)?;
    let handle = Arc::clone(&self.handle);
    let token = options.cancellation_token.cloned()
        .unwrap_or_else(CancellationToken::new_unsignalled);
    let mut guard = CancelOnDrop::armed(token.clone());

    let result = tokio::task::spawn_blocking(move || {
        let _busy = BusyGuard(&handle);     // clears busy on completion
        let raw_options = owned.as_ffi(token.raw());
        let raw = unsafe { ffi::ruau_checker_check(handle.raw, ...) };
        decode_check_result(raw, &owned.module_id) // owns + frees raw
    })
    .await
    .map_err(/* JoinError */)??;
    guard.disarm(); // success: do not cancel the caller's reusable token
    Ok(result)
}
```

**Cancellation ownership.** `tokio::time::timeout` / `select!` do not automatically cancel a
`spawn_blocking` job; dropping the `JoinHandle` leaves the closure running. The current
`CheckOptions` carries an optional borrowed `CancellationToken` and expects callers to call
`cancel()` themselves. `CancelOnDrop` signals the native token whenever the future is dropped
*before* successful completion. The guard is armed at construction and `disarm()`-ed on the
success path so a successful check does not corrupt a caller-supplied reusable token —
`CancellationToken` is explicitly reusable via `reset()`, and silently cancelling it on every
successful return would break that contract.

The size of the change is a handful of lines per call site plus the `OwnedCheckInputs` /
`Arc<CheckerHandleInner>` / `BusyGuard` / `CancelOnDrop` plumbing. The composability gain —
long checks no longer hold the runtime thread, and `timeout` / `select!` cancel the C work
correctly without leaking handles — is the actual prize.

### 3. Unify the two resolver→`require` implementations

Two parallel "wire a `ModuleResolver` up as a Luau `require` function" implementations exist:

- Runtime: `luau/mod.rs:118-191` — `install_module_resolver` /
  `resolver_require_function` / `resolver_require` / `resolver_environment`.
  `Arc<dyn ModuleResolver>`, `Rc<RefCell<HashMap<ModuleId, Value>>>` cache, async require
  (`create_async_function`).
- Checked-load: `analyzer.rs` `snapshot_environment` / `snapshot_require`.
  `Arc<ResolverSnapshot>`, `Rc<RefCell<HashMap<ModuleId, Value>>>` cache, **sync** require
  (`create_function` + `call_sync`).

Both build a per-`require` environment and a same-shaped cache. The duplication is real and
worth removing.

The two paths don't share a single call site: `install_module_resolver` writes `require` into
`lua.globals()` (no env table at all), while `checked_load` builds a per-chunk environment and
leaves the global `require` alone. But the primitives they need are already in `luau/mod.rs`:

- `resolver_require_function` (`luau/mod.rs:126-138`) — builds the `require` `Function` for any
  resolver + cache + `requester` triple.
- `resolver_environment` (`luau/mod.rs:179-194`) — builds an env table whose `__index` proxies
  globals and whose `require` is `resolver_require_function(...)`.

Make both `pub(crate)`, make `ResolverSnapshot` itself `impl ModuleResolver` (it already has a
`dependency` lookup), and have `checked_load` import them directly:

```rust
// In analyzer.rs::checked_load
let resolver: Rc<dyn ModuleResolver> = Rc::new(snapshot);
let cache = Rc::new(RefCell::new(HashMap::new()));
let env = crate::luau::resolver_environment(self, resolver, cache, Some(root_id))?;
self.load(root_source).set_name(...).set_environment(env)
```

`install_module_resolver` keeps using `resolver_require_function` directly, exactly as it does
today. One set of helpers, one cache shape, one mental model. Drops `snapshot_environment`,
`snapshot_require`, and the `Chunk::call_sync` shortcut — ~60 LOC.

### 4. Drop `Send + Sync + 'static` from `ModuleResolver`

The trait demands `Send + Sync + 'static` (`resolver.rs:144`), forcing `Arc<dyn ModuleResolver>`
storage even though the VM is `Send + !Sync` and the resolver only ever runs on the VM thread.
With `LocalSet`-based execution, drop these bounds:

```rust
pub trait ModuleResolver: 'static {
    fn resolve(...) -> StdResult<ModuleSource, ModuleResolveError>;
}
```

Internal storage becomes `Rc<dyn ModuleResolver>` — drops `Send`/`Sync` gymnastics and lets users
register `ModuleResolver`s that close over `!Send` data (an in-memory cache, a
`tokio::sync::mpsc::Sender<!Send>`, etc.).

### 5. Make resolution async-native from day one with `async_trait`

Stage 7 of `modern.md` flagged async resolution as deferred. Doing it cleanly is easier now than
later. Native `async fn` in a trait is not dyn-compatible, which conflicts with the
`Rc<dyn ModuleResolver>` storage from #4. The project is fine with adding `async_trait` for
this, so the proposal is:

```rust
#[async_trait::async_trait(?Send)]
pub trait ModuleResolver: 'static {
    async fn resolve(&self, requester: Option<&ModuleId>, specifier: &str)
        -> StdResult<ModuleSource, ModuleResolveError>;
}
```

The `?Send` form keeps resolvers in the same `!Send` world as the rest of the local-VM API.
Sync implementations (`InMemoryResolver`, `FilesystemResolver`) are `async fn`s whose body is
sync. The runtime require (built in #3) becomes a straight `async` Luau function over
`resolver.resolve(...).await`. `ResolverSnapshot::resolve` becomes `async`. No more "we'll add
an async path later" — there's only one path, and it composes with the `spawn_blocking`-style
analyzer change in #2.

**Public-API ripple.** `Luau::checked_load` currently calls `checker.check_snapshot(&snapshot)`,
and `checked_load_resolved` calls `ResolverSnapshot::resolve(resolver, root)` before delegating
(`analyzer.rs:474-516`). Under #2 (async checker) and #5 (async resolve), both become
`async fn`. That's the natural shape for a Tokio-first crate, but call it out so the breaking
change is intentional rather than incidental — see the sequencing note at the bottom.

## API surface trims (free now, no back-compat)

### 6. Drop the `set_*` prefix on `Chunk`'s consuming builders

Stage 3 already renamed `Compiler::set_*` to fluent names. `Chunk` was missed:

| Current | Suggested |
|---|---|
| `Chunk::set_name(s)` | `Chunk::name(s)` |
| `Chunk::set_environment(t)` | `Chunk::environment(t)` |
| `Chunk::set_text_mode()` | `Chunk::text_mode()` |
| `unsafe Chunk::set_binary_mode()` | `unsafe Chunk::binary_mode()` |
| `Chunk::set_compiler(c)` | `Chunk::compiler(c)` |

Symmetric with `Compiler` — only `&self` setters on `Luau` should keep `set_`.

### 7. Collapse the registry-key API

`state/mod.rs` exposes nine methods for the Luau registry: `set_named_registry_value`,
`named_registry_value`, `unset_named_registry_value`, `create_registry_value`, `registry_value`,
`remove_registry_value`, `replace_registry_value`, `owns_registry_value`,
`expire_registry_values`. All threaded directly off `Luau`.

Group them under one `lua.registry()` accessor returning a `Registry` view:

```rust
let r = lua.registry();
r.set("token", value)?;          // string key
let v: i32 = r.get("token")?;
let key = r.insert(value)?;      // RegistryKey
let v: i32 = r.get(&key)?;
r.remove(key)?;
r.expire();
```

`Registry::get` / `set` / `remove` overload `&str` and `&RegistryKey` via a small sealed
`RegistryKeyKind` trait. Drops the noun-prefix soup from the top-level methods on `Luau`.

### 8. Slim the app-data API to one borrow handle

Seven methods today (`set_app_data`, `try_set_app_data`, `app_data_ref`, `try_app_data_ref`,
`app_data_mut`, `try_app_data_mut`, `remove_app_data`). With no back-compat, this is one borrow
guard:

```rust
lua.app_data::<T>().insert(value);
lua.app_data::<T>().borrow();      // panicking
lua.app_data::<T>().try_borrow();  // returns Result<Option<…>, BorrowError>
lua.app_data::<T>().take();
```

Same internal `app_data` cell, one return type to remember. `set_app_data` returning `Option<T>`
was an awkward shape anyway.

### 9. Hide or rethink `LuauNativeFn{,Mut,AsyncFn}`

These three traits are public (re-exported at root) but exist purely as the dispatch backbone
for `Function::wrap` and friends. Users never name them. Mark `#[doc(hidden)]` and drop the root
re-exports — they're noise in rendered docs.

While you're there: `Function::wrap` / `wrap_mut` / `wrap_raw` / `wrap_raw_mut` plus
`Luau::create_function` / `create_function_mut` / `create_async_function` plus the same on
`Scope` is 11 entry points spread across three types. Document the matrix in one place; the
`wrap_*` variants probably want hiding under `Function::` since `Luau::create_function` is the
primary path.

### 10. Drop `private::Sealed` and the doc-hidden trait methods

`private::Sealed` (`lib.rs:307-318`) protects `ErrorContext`. Sealing existed for back-compat;
with no compat constraint and the trait being a tiny ergonomic helper, drop the sealing
entirely.

Same for the `#[doc(hidden)]` `push_into_stack`, `from_stack_arg`, etc. on `IntoLuau` /
`FromLuau` / `IntoLuauMulti` / `FromLuauMulti`. These leak FFI shape into a public trait. Move
them to a private extension trait (`IntoLuauStack`, `FromLuauStack`) only implemented for the
same set of types — public traits stay clean, internal hot path is the same.

### 11. Move value coercion to `Value`

`Luau::coerce_string`, `coerce_integer`, `coerce_number` are methods on the VM that take a
`Value` and produce a coerced `Value`. They belong on `Value`, taking the VM explicitly so the
primitive `Value` variants (`Nil`, `Boolean`, `Integer`, `Number`, `Vector`, `LightUserData`)
that don't carry a `WeakLuau` still work:

```rust
impl Value {
    pub fn coerce_string(&self, lua: &Luau) -> Result<Option<LuauString>>;
    pub fn coerce_integer(&self, lua: &Luau) -> Result<Option<Integer>>;
    pub fn coerce_number(&self, lua: &Luau) -> Result<Option<Number>>;
}
```

Same body as the current implementations (`state/mod.rs:1178-1247` for the three coerce
methods), just relocated. Matches the existing `Value::type_name()` pattern.

### 12. Collapse the `gc_set_mode` API

`GcMode` has a single variant (`Incremental(GcIncParams)`). `gc_set_mode(GcMode) -> GcMode`
returns a value with all `Option<_>` fields as `None` because the C API doesn't read params
back. That's a dishonest signature. Either:

- Keep the enum for future-proofing and return `()`, or
- Inline: `lua.gc_tune(GcIncParams::default().goal(200))` → `()`.

The current shape implies a getter that doesn't get.

### 13. Unify thread creation/collection callbacks

`set_thread_creation_callback` (`Fn(&Luau, Thread)`) and `set_thread_collection_callback`
(`Fn(LightUserData)`) install callbacks on the same `userthread` C hook with two separate `Rc`
slots. Collapse to one:

```rust
pub fn set_thread_callbacks(
    &self,
    on_create: impl Fn(&Self, Thread) + 'static,
    on_collect: impl Fn(LightUserData) + 'static,
)
pub fn remove_thread_callbacks(&self)
```

or a struct with `on_create` / `on_collect` `Option`s. Same wiring, half the API.

### 14. Add `From<ModuleResolveError> for crate::Error`

`ResolverSnapshot::resolve` returns `Result<Self, ModuleResolveError>`;
`AnalysisError::from(ModuleResolveError)` exists, but there's no `crate::Error` conversion.
Application code that mixes `lua.create_*` (returning `Result<…, Error>`) with resolver work
hits `?` failures. Add the impl — a one-liner `Error::external` wrap.

### 15. Drop `Compiler::vector_ctor` / `vector_type` compatibility hooks

Stage 5 explicitly kept these as `#[doc(hidden)]` "for legacy `Vector3.new` aliases". With no
back-compat and a Luau-only target where `vector.create` is canonical, just delete them.
Stage 5.2 left this open; settle it.

### 16. Reconsider `Luau::set_globals`

Documented to silently *not* affect existing functions because Luau caches the env per chunk.
That's a footgun. Realistic users want `Function::set_environment` per chunk. Either remove
`set_globals` outright or rename to `replace_globals_table` with a doc warning that this only
changes future loads.

## Internal cleanup wins

### 17. Reduce the `Checker` API

`Checker` exposes 8 methods that mostly differ only in input shape:

```
add_definitions(&str)
add_definitions_path(&Path)
add_definitions_with_name(&str, &str)
check(&str)
check_path(&Path)
check_with_options(&str, CheckOptions)
check_path_with_options(&Path, CheckOptions)
check_snapshot(&ResolverSnapshot)
```

The duplication is "string vs path" × "default options vs options". With an input enum:

```rust
pub enum CheckSource<'a> {
    Source(&'a str),
    Path(&'a Path),
    Snapshot(&'a ResolverSnapshot),
}

impl Checker {
    pub async fn check(
        &mut self,
        source: impl Into<CheckSource<'_>>,
        options: CheckOptions<'_>,
    ) -> Result<CheckResult, AnalysisError>;

    pub fn add_definitions(
        &mut self,
        defs: impl Into<DefinitionSource<'_>>,
    ) -> Result<(), AnalysisError>;
}
```

8 methods → 2. `CheckOptions` already has `Default`, so `Checker::check(src, default())` is the
no-options call. The "with_name" variant becomes a field on `CheckOptions` (already there,
called `module_name`).

### 18. Consolidate `LuauOptions` and runtime knobs

`LuauOptions` has 2 fields. `Luau::set_compiler`, `Luau::enable_jit`, `Luau::set_memory_limit`
are `&self` setters that look identical conceptually. Keep `LuauOptions` for construction, but
consider one builder method that subsumes the rest:

```rust
lua.configure(|cfg| cfg.compiler(c).jit(true).memory_limit(N))
```

Or accept the current shape and document — but right now they're a scattered bag.

### 19. `MultiValue` is barely-distinguished from `Vec<Value>`

`MultiValue(VecDeque<Value>)` exposes `Deref<Target = VecDeque<Value>>` + `DerefMut` +
`From<Vec<Value>>` + `From<MultiValue> for Vec<Value>` + `IntoIterator` + `FromIterator`.
So it's a `VecDeque<Value>` with a name. Two options:

- Keep the name (helpful for argument inference) but make it a thin wrapper with a smaller API:
  `len`, `iter`, `push_back`, `pop_front`, `from_vec`, `into_vec`. Hide the `Deref` to
  `VecDeque`.
- Or replace `MultiValue` with `Vec<Value>` everywhere (publicly) and keep `MultiValue` as an
  internal storage type. `Deref` means callers already use `Vec` semantics.

The macro-generated `IntoLuauMulti for (T1, ...)` impls don't care which is exposed.

### 20. Revisit crate-level lint allows

`lib.rs:115-120` adds `clippy::absolute_paths`, `arc_with_non_send_sync`, `items_after_statements`,
`multiple_inherent_impl` allows. The `arc_with_non_send_sync` allow is probably no longer
load-bearing once `Send`/`Sync` requirements drop in #4 above.

### 21. Don't install the FilesystemResolver by default

`luau/mod.rs:111-113`: every `Luau::new()` calls `std::env::current_dir()` and installs a
filesystem-backed `require`. That's surprising for an embedding library — many embeddings
explicitly do not want the host filesystem visible. With Stage 4's resolver consolidation done,
the default should be either a no-op or an in-memory resolver, with
`lua.set_module_resolver(FilesystemResolver::new(cwd))` opt-in. Embeddings doing checked-load
already wire their own resolver.

Bonus: `Luau::new()` becomes infallible (no fs read at construction).

## Recommended sequencing

The biggest single user-facing simplification is **#1 + #6**: every async entrypoint becomes
`async fn` and the chunk builder reads
`lua.load(src).name("main").environment(env).exec().await`, with no `AsyncCallFuture` type to
import or document.

The biggest internal cleanup is **#2 + #3 + #5**: one resolver path, async-native (with
`async_trait`), and a `spawn_blocking`-driven analyzer that no longer pins the runtime thread.
This bundle also makes `Luau::checked_load` and `Luau::checked_load_resolved` `async fn` —
intentional, because both transitively call the new async `Checker` and async resolver. Land
that group together so the public `checked_load*` signatures only break once.

The trims (#7-#16) are mostly independent and can be picked up one at a time. The last group
(#17-#21) is internal hygiene that follows from the above.

## Implementation checklist

Tick items as they land. Items are grouped by commit batch.

**Batch A — easy API trims**

- [x] #6 Drop the `set_*` prefix on `Chunk`'s consuming builders
- [x] #14 Add `From<ModuleResolveError> for crate::Error`
- [x] #15 Drop `Compiler::vector_ctor` / `vector_type` compatibility hooks
- [ ] #20 Revisit crate-level lint allows (deferred — depends on Batch C `Send`/`Sync` drop)

**Batch B — collapse `AsyncCallFuture`**

- [x] #1 Collapse `AsyncCallFuture` into plain `async fn`

**Batch C — async ModuleResolver + resolver unification**

- [x] #4 Drop `Send + Sync + 'static` from `ModuleResolver`
- [x] #5 Make resolution async-native with `async_trait`
- [x] #3 Unify the two resolver→`require` implementations

**Batch D — async Checker + checked_load ripple**

- [x] #2 Make `Checker::check` async and use `spawn_blocking`
- [x] `Luau::checked_load{,_resolved}` become `async fn`

**Batch E — mid API trims**

- [ ] #7 Collapse the registry-key API
- [ ] #8 Slim the app-data API to one borrow handle
- [ ] #11 Move value coercion to `Value`
- [ ] #12 Collapse the `gc_set_mode` API
- [ ] #13 Unify thread creation/collection callbacks
- [ ] #16 Reconsider `Luau::set_globals`
- [ ] #19 `MultiValue` vs `Vec<Value>`

**Batch F — internal hygiene**

- [ ] #9 Hide or rethink `LuauNativeFn{,Mut,AsyncFn}`
- [ ] #10 Drop `private::Sealed` and the doc-hidden trait methods
- [ ] #17 Reduce the `Checker` API
- [ ] #18 Consolidate `LuauOptions` and runtime knobs
- [ ] #21 Don't install the `FilesystemResolver` by default
