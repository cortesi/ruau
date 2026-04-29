# Post-Refactor Reap

The recent refactor centered `ruau` around Luau and Tokio async, and a series of
"Reap" batches (A–F) trimmed dead Lua-compat fields, slimmed `ruau_derive`, and
deduped userdata creation. This plan captures what is still left to reap on
top of that work — both API ergonomics that the refactor made possible but did
not yet realise, and internal complexity inherited from the multi-backend Mlua
heritage that is now pure tax.

The thesis: with a single Luau backend, a single-owner VM (`Send + !Sync`),
unconditional async, and Tokio as the execution model, several layers of
indirection no longer earn their keep, and several adapter APIs that exist only
to bridge that older complexity can be collapsed into a smaller, more
discoverable surface.

## Decisions

- **Clean break.** There are no external users to migrate, no published API
  contract to honour, and no downstream code outside this workspace. Every
  callsite — examples, tests, doctests, derive output, examples crate — is
  in this repo. "Migration" here means "fix the internal callsite in the
  same diff." No deprecation, alias, or compat shim is needed for any item
  in this plan.
- When in doubt, prune. Default toward deletion of unused, unfinished, or
  superfluous surface rather than reshaping it.
- Prefer one canonical public path for each concept. Where the refactor left a
  thin wrapper over a private inherent method, fold the body into the wrapper
  rather than keep both.
- Optimize for the common case: a current-thread Tokio runtime with
  `LocalSet`, typed Rust callbacks, host definitions, and a single resolver.
- Internal `Arc`/`Mutex` machinery exists to support `Send` on `Luau`. The
  documented usage model is single-threaded; we treat the `Send` bound as
  removable and account for the atomic and locking cost everywhere it
  propagates.
- Keep behaviour. None of these items change observable semantics for code
  that already follows the documented usage; they trim API surface and
  internal mass.

## A. API Ergonomics

### A1. Delete the `Function::wrap*` family

`crates/ruau/src/function.rs:471–589` exposes `wrap`, `wrap_mut`, `wrap_raw`,
`wrap_raw_mut`, `wrap_async`, `wrap_raw_async`, backed by three hidden traits
`LuauNativeFn` / `LuauNativeFnMut` / `LuauNativeAsyncFn` plus a 16-arity macro
for each (`function.rs:639–701`). The 16-arity macros exist only to spread
tuples into closure args. With `FromLuauMulti` already implemented for tuples,
the same effect is reachable today with the `(&Luau, A) -> R` callbacks
created by `Luau::create_function` / `create_function_mut` /
`create_async_function`. The only thing the wrap family adds is the right to
omit the `&Luau` parameter.

A unifying `Function::wrap` backed by a sealed `IntoCallback` trait covering
`Fn` / `FnMut` / `AsyncFn` is not implementable as written: every `Fn` closure
also implements `FnMut`, so blanket impls collide without specialization or a
caller-visible disambiguator. Designing around that is more code and more
documentation than the sugar saves, and no in-repo example uses
`wrap_raw_async` or `wrap_raw_mut`.

Recommendation: delete the entire `wrap*` family along with the
`LuauNativeFn` / `LuauNativeFnMut` / `LuauNativeAsyncFn` traits and their
arity macros. Keep `create_function` / `create_function_mut` /
`create_async_function` on `Luau` as the canonical path. The `_raw`
"no-throw" flavour, if anyone needs it, can be expressed as a manual
`Result` wrap inside the closure rather than a parallel API surface.

In-repo callsites to fix in the same diff:

- `crates/ruau/tests/function.rs`, `tests/types.rs`, `tests/chunk.rs`,
  `tests/async.rs` — all current `Function::wrap*` users.
- `crates/ruau/src/function.rs` module-level docstring examples
  (`function.rs:49`, `:60`).

`UserDataFields::add_meta_field<V: IntoLuau + 'static>` is the only
registration site without `&Luau` in scope. The current codebase has one
`add_meta_field` call (`tests/userdata.rs:521`) and it stores a `HashMap`,
not a function, so no rewrite is needed. If a future caller wants to store a
function as a meta field, they should use `add_meta_field_with(name, |lua|
lua.create_function(...))` — call out that path in the migration commit
message rather than carry a parallel `wrap` API to support it.

### A2. Drop `WrappedFunction` / `WrappedAsyncFunction` newtypes

`function.rs:468–469`:

```rust
struct WrappedFunction(pub(crate) Callback);
struct WrappedAsyncFunction(pub(crate) AsyncCallback);
```

These exist only so `Function::wrap*` can return `impl IntoLuau`. With A1
deleting the wrap family, the newtypes have no remaining caller and can be
removed alongside.

### A3. `HostApi`: add namespaces and tables, not just globals

`crates/ruau/src/host.rs` only offers `global_function` and
`global_async_function`. Real host APIs come in namespaces — embedders
building `verber.term.print(...)` etc. drop down to `lua.create_table()` /
`globals().set(...)` and end up writing the same loop in every project.

Recommendation:

```rust
HostApi::new()
    .namespace("term", |ns| {
        ns.function("print", print_fn, "function(s: string)")
            .function("clear", clear_fn, "function()")
    })
    .global_function("log", log_fn, "function(msg: string)");
```

The new `HostNamespace` builder owns its own per-function signature strings
and emits the matching `declare term: { print: (...) -> ..., ... }` text
itself. This does not require extending `module_schema.rs` —
`NamespaceSchema` only stores function names and child namespaces today and
is intentionally signature-free; treating it as a definition generator would
be a larger and separate change than this plan covers. The installer
constructs the table, marks it `set_readonly`, and assigns it to the global
in one pass.

### A4. `lua.scope` is broken in the guided tour

`crates/ruau/examples/guided_tour.rs:219–242` has the scope demo commented out
as `// TODO: Re-enable this`. Tests in `tests/scope.rs` use it, so the API
works, but the showcased example does not. Either fix it (looks like a borrow
check issue with the outer `lua.globals()` versus the scope) or change the
guided tour so it does not advertise scope as a feature.

### A5. `Registry` is a thin wrapper over private inherent methods

`state/mod.rs:129–190` introduces a `Registry<'a>` with `named_set`, `insert`,
`get`, etc.; every `pub(crate)` underlying method on `Luau`
(`set_named_registry_value`, `create_registry_value`, `registry_value`, lines
1257–1435) is invoked through it. `Luau::registry()` is the only public path,
so the inherent methods are private — good. Just consolidate: move the bodies
into `Registry` and drop the `pub(crate)` shims, or vice versa. As written, the
indirection is empty.

### A6. Remove `ObjectLike::get_path` and the path DSL

`crates/ruau/src/traits.rs:202` parses paths like `a[1]?.c` via
`parse_lookup_path` (`util/path.rs`, 271 lines). It is a 100%-bespoke mini DSL
with no equivalent on the Luau side, partial escape-sequence support, and only
one query feature (`?` for safe-nav). It looks like an experiment that was not
pruned, and the pruning bias resolves the ambiguity: delete it.

Recommendation: remove `get_path` from `ObjectLike` and delete
`util/path.rs`. Callers that need chained access can use `.get` directly;
that is two lines and does not need a custom parser.

### A7. Hide `RawLuau` behind an opaque stack context

`state/raw.rs:47` declares `pub struct RawLuau` and `state/mod.rs:25`
re-exports it. Its only public method is `state()` returning
`*mut ffi::lua_State`. Everything else is `pub(crate)` or `unsafe`. It should
not appear in the public API at all.

The complication is that `RawLuau` is referenced in the stack-level method
signatures on `IntoLuau` / `FromLuau` / `IntoLuauMulti` / `FromLuauMulti`
in `traits.rs`. Two designs that look attractive at first do not work:

- **Sealing the public conversion traits.** `ruau_derive` emits
  `impl ::ruau::FromLuau for UserType`, the guided tour and
  `tests/userdata.rs` implement `FromLuau` for their own types, and the
  documented user pattern for custom userdata depends on
  user-implementable `IntoLuau` / `FromLuau`. Sealing breaks that.
- **Extracting separate `pub(crate) trait PushIntoStack` / `FromStack`
  with a blanket `impl<T: IntoLuau> PushIntoStack for T`.** Stable Rust
  does not allow blanket impls plus per-type override impls (no
  specialisation), so the optimised per-type stack overrides currently in
  `conversion.rs` (e.g. `LuauString`, `BorrowedStr`, `Table`, `bool`,
  `LightUserData`, `Vector`, `Either`) and the per-shape overrides in
  `multi.rs` (e.g. `StdResult<T, E>`, tuple impls) cannot coexist with a
  blanket internal impl.

Workable design: keep the stack-level methods on the public conversion
traits, but stop naming `RawLuau` in their signatures by introducing a
public-but-opaque context wrapper.

```rust
// traits.rs
pub struct StackCtx<'a> {
    pub(crate) lua: &'a crate::state::RawLuau,
}

pub trait IntoLuau: Sized {
    fn into_luau(self, lua: &Luau) -> Result<Value>;

    #[doc(hidden)]
    unsafe fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        ctx.lua.push_value(&self.into_luau(ctx.lua.lua())?)
    }
}
```

Properties:

- `StackCtx` is `pub` so it may appear in the public trait signature. Its
  field is `pub(crate)`, so external code can take a `&StackCtx<'_>` but
  cannot deconstruct it or call any internal API through it. External
  trait impls get nothing useful out of the override — they should and
  will rely on the default body, which forwards through the `&Luau` API
  via `ctx.lua.lua()`.
- All internal optimised overrides in `conversion.rs` and `multi.rs` keep
  working unchanged in shape; their body just rebinds `let lua =
  ctx.lua;` and proceeds as today. No coherence conflict, no
  specialisation needed.
- `RawLuau` becomes `pub(crate)`. The public surface no longer mentions
  it. With clean break we have no external user that needs a raw
  `*mut ffi::lua_State`, so do not add a public `Luau::raw_state()`
  accessor; if a future need surfaces it can be re-introduced
  deliberately.
- `ruau_derive`-generated code keeps compiling because it only emits
  `impl FromLuau for X { fn from_luau(...) -> ... }` — it never
  overrides `from_stack`.

The mechanical work is: change every `&RawLuau` parameter to
`&StackCtx<'_>` in the trait method signatures and in every internal
caller; thread `StackCtx { lua: rawlua }` through every callsite that
currently passes `rawlua: &RawLuau` into a stack method.

### A8. Drop the `install_resolver` helper proposal

The original draft proposed
`install_resolver(&mut Checker, &Luau, Rc<dyn ModuleResolver>)` to share a
resolver between analyzer and runtime. This is not necessary:

- `Luau::set_module_resolver<R: ModuleResolver>(R)` already accepts
  `Rc<dyn ModuleResolver>` because `resolver.rs:158` provides
  `impl<T: ModuleResolver + ?Sized> ModuleResolver for Rc<T>`.
- The "resolve once" half of the work is already
  `ResolverSnapshot::resolve(&resolver, root)` plus
  `Luau::checked_load(&mut checker, snapshot)`, or the one-shot
  `Luau::checked_load_resolved(&mut checker, &resolver, root)`.
- `Checker` does not store a persistent resolver, so a single helper that
  "wires both" cannot be specified without also fixing the root module to
  resolve, which is the caller's choice anyway.

Resolution: drop A8 from the plan. If a future helper proves useful, it
should be designed against an explicit "resolve root and install for live
require" use case, not a generic `install_resolver` shape.

### A9. Remove deprecated `Value::as_str` / `as_string_lossy`

`value.rs:413` and `:425` carry deprecation notes since 0.11. Delete them.

### A10. Remove the `anyhow` feature

`Cargo.toml:24` and the integration in `error.rs:498` plus `IntoLuau`
(`conversion.rs:330`) only provide a `From<anyhow::Error>` and an `IntoLuau`.
The pruning bias resolves the question: delete the feature flag, the
`From<anyhow::Error>` impl, and the `IntoLuau` impl.

The chain-flattening logic in `error.rs:499–515` is the genuinely useful
part; if it ever proves needed, it is a few lines and can be re-introduced
as a standalone helper without re-introducing the feature flag.

## B. Internal Simplifications

### B1. `XRc = Arc` is overkill — use `Rc` for VM-internal handles

`crates/ruau/src/types/sync.rs` is literally
`pub type XRc<T> = Arc<T>;`. Since:

- `Luau` is `Send + !Sync`,
- futures produced by `Luau` are `!Send`,
- callbacks (`Callback`, `AsyncCallback`) are non-`Send` boxed `dyn`s,
- `ValueRef`, `Thread`, `Function`, etc. are explicitly `!Send`,

…there is no scenario where these `XRc`s are dropped on a thread different
from the one `Luau` lives on at any moment. The atomic ref-counting is paid
for everywhere in `state/extra.rs`, `types/mod.rs:73-79` (callbacks),
`types/value_ref.rs:22` (`ValueRefIndex`), `state/raw.rs:51`, etc.

The blocker is that `Luau` itself is `Send` (`state/mod.rs:1767`) and so is
`RawLuau` (`unsafe impl Send for RawLuau` at `state/raw.rs:93`). When a
`Luau` moves between threads, every `Rc` inside moves with it — sound, but
`Rc` is not `Send`. Drop the `Send` impl on both `Luau` and `RawLuau` and
let the documented `current_thread` Tokio runtime + `LocalSet` story stand
on its own.

Liveness tracking is still required, just not atomic. Public handles such
as `WeakLuau`, `RegistryKey`, `ValueRef`, `Function`, and `Thread` can
outlive their owning `Luau`; `Luau::drop` flips a flag and `ValueRef::drop`
checks it via `WeakLuau::try_raw()` (`types/value_ref.rs:55-64`,
`state/mod.rs:267-274`, `state/mod.rs:1736-1742`). Replace
`Arc<AtomicBool>` with `Rc<Cell<bool>>` (and `Weak<AtomicBool>` with
`Weak<Cell<bool>>`) so the validation stays but loses the atomic op. Update
`registry_unref_list: Arc<Mutex<Option<Vec<c_int>>>>` (`state/extra.rs:43`)
to `Rc<RefCell<Option<Vec<c_int>>>>` in the same pass.

The crate docs at `lib.rs:45` currently advertise `Luau` as
`Send + !Sync`; this stage must rewrite that prose to `!Send + !Sync`.
Static assertions for both `Luau` and `RawLuau` need to flip from `Send` to
`!Send`.

### B2. `RegistryKey: Send + Sync` is inconsistent and forces `Arc<Mutex<...>>`

`crates/ruau/src/types/registry_key.rs:104` asserts `Send + Sync`. Combined
with B1, this forces
`unref_list: Arc<Mutex<Option<Vec<c_int>>>>` even though every other Luau
handle is `!Send`. Drop `Send + Sync` on `RegistryKey` and the mutex collapses
to `RefCell` (alongside the `Rc<Cell<bool>>` change in B1).

### B3. `WeakLuau` and `LuauLiveGuard` are near-duplicates

`state/mod.rs:64–68` (`WeakLuau`) and `state/mod.rs:70–74` (`LuauLiveGuard`)
carry the same fields. `LuauLiveGuard` is `Deref<Target = RawLuau>` and used
to keep liveness for `AppDataRef`/`Scope` borrows. `WeakLuau` is the
user-visible weak handle.

Recommendation: make `LuauLiveGuard` a typedef for `WeakLuau` (or a thin
wrapper), or upgrade `WeakLuau` once at borrow time and store the upgraded
liveness handle (a true RAII guard rather than re-checking on each deref).
Today the deref checks the `AtomicBool` on every access
(`state/mod.rs:1756–1759`); after B1 this becomes a non-atomic
`Cell<bool>` load, and merging the types lets that load happen once at
borrow-create rather than once per field access.

### B4. `UserDataRefInner` and `UserDataRefMutInner` enums have one variant each

`crates/ruau/src/userdata/ref.rs:101–103` and `205–207` each define a
single-variant `enum ... { Default(UserDataVariant<T>) }` with
`#[allow(unused)]`. Vestigial from the refactor — flatten:

```rust
pub struct UserDataRef<T: 'static> {
    _guard: LockGuard<'static, RawLock>,
    inner: UserDataVariant<T>,
}
```

Drops two enums, four `match` arms, eight lines of indirect deref each.

### B5. Lift the serializable-userdata variant out of `UserDataVariant`

`crates/ruau/src/userdata/cell.rs:18` is
`Owned(UserDataVariant<T>) | Scoped(ScopedUserDataVariant<T>)`. The
`Default` / `Serializable` split inside `UserDataVariant` (`cell.rs:24–28`)
adds an extra variant whose only difference from `Default` is "the boxed
value is `dyn Serialize`". Every borrow path matches on this tag. After
B12 lands (serde unconditional), the feature gate disappears but the
runtime tag does not — fixing the dispatch shape is a separate,
structural change.

The cleanest fix is structural, not erased: keep `UserDataVariant` as a
single shape (the current `Default`), and move the serializable case into a
separate type used only by `create_ser_userdata` / `create_ser_any_userdata`.
The shared serialization plumbing then routes through that distinct type
rather than through a runtime-checked enum on every borrow.

The earlier draft also floated "store all userdata as
`Box<dyn erased_serde::Serialize>` plus an is-serializable bit". That is not
type-correct: `create_userdata<T: UserData + 'static>` does not require
`T: Serialize`, so non-serializable `T` cannot be erased through
`dyn Serialize`. Discard that option.


### B7. Delete the bespoke JSON parser, use `serde_json`

`crates/ruau/src/luau/json.rs` (346 lines) is consumed solely by
`heap_dump.rs` to walk the dump produced by `lua_gcdump`. With serde
unconditional (B12), the embedder is already paying for `serde` plus
`erased-serde` plus `serde-value`; adding `serde_json` to that group is a
small marginal cost.

Recommendation: delete `crates/ruau/src/luau/json.rs` outright. Add
`serde_json` as an unconditional dependency in `Cargo.toml`. Rewrite the
five accessors in `heap_dump.rs` (`size`, `size_by_type`,
`size_by_category`, `size_by_userdata`, `find_category_id`) to walk the
parsed `serde_json::Value` directly via `get(key)` / `as_str()` /
`as_u64()` / `as_object()`. The parsed structure is simple and the heap
dump is not on a hot path, so the loss of zero-copy `&str` borrowing into
the source buffer is acceptable.

If for some reason `serde_json` is rejected, the fallback is to trim the
bespoke parser by hand: drop the `Index<&str>` machinery and the
`&Json::Null` sentinels (`json.rs:17–32`), replace them with explicit
`as_object()?.get(key)` access in `heap_dump.rs`, and shrink the parser to
the minimum needed for the existing methods.

### B8. `LuauLiveGuard` re-checks liveness on every access

Already noted in B3. Specifically, every userdata borrow, every
`app_data_ref`, every value access goes through `Deref::deref`, which does
an `Arc::upgrade` plus `AtomicBool::load`. After B1 the load becomes a
non-atomic `Cell<bool>` read; folding B3's "upgrade once at borrow-create"
on top eliminates the per-access check entirely for the duration of a
single guard. If for some reason B1 lands without B3, at least cache the
upgraded liveness handle inside the guard so each access is one relaxed
load rather than an upgrade plus a load.

### B9. `Luau::raw()` panics if `extra.running_gc`

`state/mod.rs:1681` re-checks `running_gc` on every safe API call. The flag is
set only by the `userthread` GC callback (`state/mod.rs:545`). In normal flow
this branch is never true, so it is a runtime cost on the steady state to
defend against a pathological reentry. Make it `debug_assert!`, or move the
check to where reentry is actually possible (the userthread hook itself).

### B10. Dead `pub use callback_error_ext` from `state/mod.rs`

`state/mod.rs:28` — `pub use util::callback_error_ext;` and `state/raw.rs:21`
imports it. This is internal — gate it `pub(crate)`.

### B11. Document `AsyncThread::recycle` as the provenance gate

`thread.rs:499` `pub(crate) fn set_recyclable(...)` is called only at
`function.rs:202` with `true`. `Thread::into_async` keeps `recycle = false`
for the `AsyncThread` it returns. The flag is the gate that distinguishes
threads spawned through `Function::call` (which come from
`create_recycled_thread`) from threads users obtained via
`Thread::into_async` and may have customised — sandboxed via
`Thread::sandbox` (`thread.rs:466-474`), reset, given a different
`LUA_GLOBALSINDEX`, etc. `reset_inner` (`thread.rs:338-355`) resets status
and stack, and `create_recycled_thread` re-assigns the caller's globals on
checkout (`state/raw.rs:507-516`), but neither path proves that a tainted
`Thread::into_async` thread has been normalised before pooling.

Recommendation, revised:

- Default direction: **keep the flag**, document it inline in `thread.rs`
  as the provenance gate for the recycled-thread pool, and stop describing
  this as an "easy deletion" candidate.
- Optional follow-up: if a full tainted-thread regression is added
  (sandbox or custom-globals → `Thread::into_async` → drop → later
  `Function::call` proving no state leaks), then `recycle` may be removed
  in favour of the pool-size check at `state/raw.rs:527` alone. Without
  that proof, the flag stays.

This item moves out of the "easy deletions" stage; see the checklist.

### B12. Make serde unconditional

`Cargo.toml:22` gates serde behind a feature flag along with three of its
dependents (`serde`, `erased-serde`, `serde-value`, plus `bstr/serde`).
The crate already depends on these in every realistic embedding —
userdata serialisation, the `Value: Serialize` impl, the heap-dump JSON
walker (B7), and the typed conversion helpers all need them. The feature
gate exists mainly to support the compile-time sliver where a user wanted
ruau without serde, which the clean-break decision rules out: there is no
such user.

Recommendation: drop the `serde` feature entirely.

- `Cargo.toml`: move `serde`, `erased-serde`, `serde-value` from
  `[dependencies]` `optional = true` to required. Drop the `serde`
  feature stanza. Make `bstr` carry the `serde` feature unconditionally.
  Update `[package.metadata.docs.rs]` `features = ["serde", "macros"]` →
  `features = ["macros"]`.
- Strip every `#[cfg(feature = "serde")]` and
  `#[cfg_attr(docsrs, doc(cfg(feature = "serde")))]` annotation from the
  crate. The current sites span `lib.rs`, `error.rs` (the
  `SerializeError` / `DeserializeError` variants and the
  `serde::ser::Error` / `serde::de::Error` impls), `value.rs`
  (`SerializableValue` and the `Value: Serialize` impl), `table.rs`
  (`Table: Serialize`), `userdata/cell.rs` (the `Serializable` variant
  and the `UserDataStorage<()>: Serialize` impl), `userdata/mod.rs`,
  `state/mod.rs` (`create_ser_userdata`, `create_ser_any_userdata`),
  `multi.rs`, and the `serde/` module itself. About 30+ annotations
  crate-wide.
- Fold `LuauSerdeExt` (`crates/ruau/src/serde/mod.rs`) into inherent
  methods on `Luau`. The trait exists only to bolt `to_value` /
  `from_value` / `from_value_with` etc. onto `Luau` from another module;
  with serde always-on there is no reason for the indirection. Drop the
  `LuauSerdeExt` trait, the `pub use crate::serde::LuauSerdeExt`
  re-export in `lib.rs`, and the `use ruau::LuauSerdeExt;` import that
  callsites currently need. Real ergonomic win — the serde API becomes
  discoverable on the `Luau` rustdoc page.
- Optional follow-up: audit `SerializableValue` callsites. If its sole
  reason for existing is to thread `SerializeOptions` per call, consider
  whether a sensible default `Value: Serialize` impl plus
  `lua.to_value_with(value, opts)` covers the use cases. If yes, drop
  `SerializableValue`. Verify against the test suite first; this is a
  conditional win and need not gate the rest of B12.

Validation matrix shrinks: with B12 plus A10 (anyhow removal), the only
remaining feature is `macros`, so `cargo xtask test`'s feature-combination
sweep collapses to `default` and `default + macros`.

## Suggested Ordering

The plan groups easy deletions first to build momentum, then small public
tightenings, then the larger structural plays.

1. **Easy deletions (B4, B10)** — vestigial enums and a stray `pub use`.
   Pure deletions, near-zero risk, ~30 lines gone.
2. **Public-API tightening (A5, A9, B9)** — fold `Registry` and inherent
   methods, remove deprecated `Value` accessors, demote `running_gc`
   reentry guard.
3. **Drop `async-trait` (B6)** — one trait rewrite, kills a proc-macro dep.
4. **Hide `RawLuau` (A7)** — introduce `pub struct StackCtx<'a>` with a
   `pub(crate)` field over `&RawLuau`, swap `&RawLuau → &StackCtx<'_>` in
   the public conversion-trait method signatures and at every internal
   stack callsite, then demote `RawLuau` to `pub(crate)`. Sweep
   `function.rs`, `state/mod.rs`, `state/raw.rs`, `scope.rs`,
   `userdata/registry.rs`, `userdata/ref.rs`, `multi.rs`, `table.rs`,
   `thread.rs`, and `conversion.rs` (and `tests/` and example crates) so
   nothing still names `&RawLuau` from a sealed-trait method.
5. **Wrap-family deletion (A1, A2)** — delete the six `wrap*` entry points
   and the `LuauNativeFn*` traits along with the `WrappedFunction` /
   `WrappedAsyncFunction` newtypes. Rewrite the in-repo callsites in
   `tests/` and the `function.rs` doc examples to `create_function*`.
6. **Big internal play (B1, B2, B3, B8)** — drop `Send` on `Luau` and
   `RawLuau`, fold `Arc → Rc`, swap `AtomicBool` for `Cell<bool>`,
   simplify `WeakLuau` / `LuauLiveGuard`. Touches many files but the diff
   should net out smaller. Update crate docs at `lib.rs:45` and the
   static-assertion blocks for both `Luau` and `RawLuau`.
7. **HostApi namespaces (A3)** — biggest external-ergonomics win once the
   surrounding cleanup has landed.
8. **Tested simplifications and opportunistic cleanup (A4, A6, A10, B5,
   B7, B11, B12)** — sequence inside this group is mostly free, but B12
   should land before B5 and B7 because both items reference an
   already-unconditional serde stack (B5's prose drops the "feature-gated"
   framing; B7 swings to "use `serde_json`" once the dep can be assumed).
   B11 defaults to keeping the flag; only the optional removal path needs
   the tainted-thread regression test called out in its item.

## Validation

Each stage finishes when:

- `cargo fmt --all` is clean.
- `cargo xtask test` passes (covers `cargo test`, doc tests, and example
  builds across feature combinations).
- `cargo xtask tidy` is clean.
- For API-changing stages, the guided tour, `tokio_embed`, and `userdata`
  examples build and run without warnings.

For Stage 4 (A7) specifically:

- `cargo doc -p ruau --no-deps --all-features` succeeds with no public-API
  references to `RawLuau`.
- `ruau_derive`-generated code, the guided tour's manual `FromLuau` impl,
  and `tests/userdata.rs` continue to compile without seeing `RawLuau` or
  needing to construct a `StackCtx`.
- The sweep `rg "&RawLuau|RawLuau\)" crates/` returns only matches inside
  the crate's `pub(crate)` modules; no public signature mentions
  `RawLuau`, and no test or example file references it.
- The sweep `rg "from_stack_args|push_into_stack_multi|push_into_stack\\(|from_stack\\(" crates/`
  returns no callsite that still uses the old `&RawLuau` parameter shape;
  every match takes `&StackCtx<'_>`.

For Stage 6 (B1+B2+B3+B8) specifically:

- `cargo doc -p ruau --no-deps --all-features` is link-clean.
- `static_assertions` blocks for `Luau`, `RawLuau`, `WeakLuau`,
  `RegistryKey`, `Function`, `Thread`, and `AsyncThread` reflect the new
  Send/Sync posture (i.e. `Luau` and `RawLuau` are `!Send`).
- The crate-level docs at `lib.rs:45` and the README no longer claim
  `Luau: Send`.
- A direct test that drops a `Luau` while a `RegistryKey` and a `WeakLuau`
  are still alive and exercises their post-drop behaviour, confirming the
  `Rc<Cell<bool>>` liveness tracking still gates access correctly.

For Stage 8 (after B12 + A10 land):

- The `cargo xtask test` feature-combination sweep collapses to `default`
  and `default + macros`. Confirm CI is updated to match.
- Rendered docs no longer carry `cfg(feature = "serde")` decorations on
  any item; `cargo doc -p ruau --no-deps` succeeds without
  `--features serde`.
- `LuauSerdeExt` is gone from the public API; serde methods appear under
  `impl Luau` in rustdoc.

## Staged Execution Checklist

### Stage 1 — Easy Deletions

- [ ] B4. Flatten `UserDataRefInner` and `UserDataRefMutInner` into the
      surrounding structs (`crates/ruau/src/userdata/ref.rs`).
- [ ] B10. Demote `pub use util::callback_error_ext` in
      `crates/ruau/src/state/mod.rs:28` to `pub(crate)` and fix any internal
      callers.
- [ ] Validate Stage 1: `cargo fmt --all`, `cargo xtask test`,
      `cargo xtask tidy`.

### Stage 2 — Public-API Tightening

- [ ] A5. Consolidate `Registry<'a>` and the private inherent registry methods
      on `Luau` into a single home; remove the duplicate layer.
- [ ] A9. Delete `Value::as_str` and `Value::as_string_lossy`
      (`crates/ruau/src/value.rs:413`, `:425`).
- [ ] B9. Demote the `running_gc` reentry check to `debug_assert!` or move it
      into the `userthread` hook.
- [ ] Validate Stage 2.

### Stage 3 — Drop async-trait

- [ ] B6. Replace `#[async_trait::async_trait(?Send)] trait ModuleResolver`
      with the explicit
      `fn resolve<'a>(&'a self, requester: Option<&'a ModuleId>, specifier: &'a str) -> Pin<Box<dyn Future<Output = ...> + 'a>>`
      signature.
- [ ] Update `InMemoryResolver`, `FilesystemResolver`, `ResolverSnapshot`,
      and the `impl<T: ModuleResolver + ?Sized> ModuleResolver for Rc<T>`
      blanket impl to the new signature.
- [ ] Remove the `async-trait` workspace and dev dependency entries.
- [ ] Validate Stage 3.

### Stage 4 — Hide `RawLuau`

- [ ] A7. Introduce `pub struct StackCtx<'a> { pub(crate) lua: &'a RawLuau }`
      in `traits.rs`. `RawLuau` is referenced only through the
      `pub(crate)` field.
- [ ] A7. Change every stack-level method on `IntoLuau` / `FromLuau` /
      `IntoLuauMulti` / `FromLuauMulti` from `&RawLuau` to
      `&StackCtx<'_>`, and update each default body to bind
      `let lua = ctx.lua;` and proceed as before.
- [ ] A7. Update every internal stack callsite to construct or pass
      `&StackCtx<'_>`. Sweep `function.rs`, `state/mod.rs`,
      `state/raw.rs`, `scope.rs`, `userdata/registry.rs`,
      `userdata/ref.rs`, `multi.rs`, `table.rs`, `thread.rs`,
      `conversion.rs`, and `luau/mod.rs` (the `lua_loadstring` callback
      at `luau/mod.rs:253-263` calls `from_stack_args` and
      `push_into_stack` with a raw `&RawLuau`). The audit `rg
      "from_stack_args|push_into_stack_multi|push_into_stack\\(|from_stack\\("`
      should return no remaining `&RawLuau` parameters.
- [ ] A7. Update every per-type stack override in `conversion.rs` and
      every per-shape impl in `multi.rs` to take `&StackCtx<'_>`. No
      blanket internal trait is introduced.
- [ ] A7. Confirm `ruau_derive` output and manual user impls (guided tour,
      `tests/userdata.rs`) still compile; they should not need to mention
      `StackCtx`.
- [ ] A7. Make `RawLuau` `pub(crate)`. Demote the `state/mod.rs:25`
      re-export. Do not add a `Luau::raw_state()` accessor — clean break,
      no in-tree caller has external need.
- [ ] Validate Stage 4 including
      `cargo doc -p ruau --no-deps --all-features`, the two `rg` sweeps
      above, and confirm the rendered docs no longer mention `RawLuau`.

### Stage 5 — Delete the `wrap*` Family

- [ ] A1. Delete `Function::wrap`, `wrap_mut`, `wrap_raw`, `wrap_raw_mut`,
      `wrap_async`, `wrap_raw_async` and the
      `LuauNativeFn` / `LuauNativeFnMut` / `LuauNativeAsyncFn` traits plus
      their 16-arity macros (`crates/ruau/src/function.rs:471–701`).
- [ ] A2. Delete `WrappedFunction` and `WrappedAsyncFunction`
      (`function.rs:468–469`) and any `IntoLuau` impls on them.
- [ ] Rewrite every in-repo callsite (`tests/function.rs`, `tests/types.rs`,
      `tests/chunk.rs`, `tests/async.rs`, `function.rs` doc examples) to
      `Luau::create_function` / `create_function_mut` /
      `create_async_function`.
- [ ] Validate Stage 5.

### Stage 6 — Internal Send/Rc Cleanup

- [ ] B1. Drop the `Send` impl on `Luau` (`state/mod.rs:1767`) and
      `RawLuau` (`state/raw.rs:93`).
- [ ] B1. Migrate `XRc` to `Rc` in `crates/ruau/src/types/sync.rs` and
      update every internal user (`state/extra.rs`, `state/raw.rs`,
      `types/mod.rs`, `types/value_ref.rs`, etc.). Keep the alias for now;
      rename in a later pass.
- [ ] B1. Replace `Arc<AtomicBool>` / `Weak<AtomicBool>` liveness handles
      with `Rc<Cell<bool>>` / `Weak<Cell<bool>>`. Audit `Luau::drop`,
      `WeakLuau::is_alive`, `WeakLuau::try_raw`, and `ValueRef::drop` for
      the new types.
- [ ] B2. Drop `Send + Sync` on `RegistryKey`; replace
      `Arc<Mutex<Option<Vec<c_int>>>>` with
      `Rc<RefCell<Option<Vec<c_int>>>>`.
- [ ] B3/B8. Merge `LuauLiveGuard` and `WeakLuau` (or make the guard cache
      the upgraded liveness handle) so deref does not re-check liveness on
      every access.
- [ ] Update crate-level docs at `lib.rs:45` and the README so they no
      longer advertise `Luau: Send`. Update `static_assertions` blocks for
      `Luau` and `RawLuau`.
- [ ] Add a post-drop liveness regression test (drop `Luau`, then exercise
      a held `RegistryKey` and `WeakLuau`).
- [ ] Validate Stage 6, including `cargo doc --all-features` and a full
      `cargo xtask test` pass.

### Stage 7 — HostApi Namespaces

- [ ] A3. Add `HostApi::namespace(name, |ns| ...)` plus an `HostNamespace`
      builder with `function` / `async_function` / nested `namespace`. The
      builder owns its own per-function signature strings.
- [ ] Generate the matching `declare ns: { ... }` declaration text directly
      from the builder; do not extend `module_schema.rs`.
- [ ] Have the installer create read-only host tables and assign them in
      one pass.
- [ ] Update the guided tour and `tokio_embed` example to use the new
      namespace API.
- [ ] Validate Stage 7.

### Stage 8 — Tested Simplifications and Opportunistic Cleanup

- [ ] B12. `Cargo.toml`: move `serde`, `erased-serde`, `serde-value` from
      optional to required, drop the `serde` feature stanza, make
      `bstr/serde` unconditional, and update `[package.metadata.docs.rs]`
      to `features = ["macros"]`.
- [ ] B12. Strip every `#[cfg(feature = "serde")]` and
      `#[cfg_attr(docsrs, doc(cfg(feature = "serde")))]` annotation from
      the crate (`lib.rs`, `error.rs`, `value.rs`, `table.rs`,
      `userdata/cell.rs`, `userdata/mod.rs`, `state/mod.rs`, `multi.rs`,
      and the `serde/` module).
- [ ] B12. Fold `LuauSerdeExt` (`serde/mod.rs`) into inherent methods on
      `Luau`. Delete the trait, the `pub use crate::serde::LuauSerdeExt`
      re-export, and any `use ruau::LuauSerdeExt;` callsites in tests,
      doctests, and examples.
- [ ] B12. Optional: audit `SerializableValue` callsites. If a default
      `Value: Serialize` impl plus `lua.to_value_with(value, opts)` covers
      the use, drop `SerializableValue`. Otherwise leave it.
- [ ] A4. Re-enable the `lua.scope` block in `examples/guided_tour.rs`
      (or remove the section if scope is no longer the recommended
      pattern).
- [ ] A6. Delete `ObjectLike::get_path`, `crates/ruau/src/util/path.rs`,
      and its test coverage; update any internal callers to chain `.get`.
- [ ] A10. Remove the `anyhow` cargo feature, the `From<anyhow::Error>`
      impl in `error.rs`, and the `IntoLuau` impl for `anyhow::Error` in
      `conversion.rs`.
- [ ] B5. Move the serializable-userdata case into a separate type used
      only by `create_ser_userdata` / `create_ser_any_userdata`; remove
      the `Default` / `Serializable` enum variants from `UserDataVariant`.
      With B12 already landed, the change is uniform — no `#[cfg]`
      branches anywhere.
- [ ] B7. Add `serde_json` as an unconditional dependency in
      `Cargo.toml`. Delete `crates/ruau/src/luau/json.rs`. Rewrite the
      five accessors in `heap_dump.rs` to walk a `serde_json::Value`
      directly. Fallback if `serde_json` is rejected: hand-trim the
      bespoke parser instead.
- [ ] B11. Default direction: keep `recycle` and document it in
      `thread.rs` as the provenance gate for the recycled-thread pool.
      Optional removal path: land a tainted-thread regression test
      (sandboxed/custom-globals `Thread::into_async` → drop → later
      `Function::call` confirming no state leaks) and only then remove
      the flag in favour of the pool-size check at `state/raw.rs:527`.
- [ ] Final validation: `cargo fmt --all`, `cargo xtask test`,
      `cargo xtask tidy`, `cargo doc -p ruau --no-deps --all-features`.
