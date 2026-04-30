# Tokio-Only Async Cleanup

`ruau` now treats Tokio as its only supported async runtime. The API should make
that contract explicit without forcing every embedder onto a single-thread
application runtime: direct `Luau` use is local and `!Send`, while idiomatic
multi-thread Tokio applications should get a `Send` worker handle that schedules
work onto a VM-owned local lane.

## Design constraints

These hold across all stages and should be revisited only if a stage forces a
re-evaluation.

- **Direct mode is preserved.** Stages Two and Three are purely additive. The
  existing `Luau` API, its `!Send + !Sync` guarantee, and all current handle
  types stay exactly as they are.
- **One VM per worker.** `LuauWorker` owns exactly one `Luau` VM. Multi-VM
  pooling, sharding, or per-request VMs are out of scope for this plan and can
  be layered above the worker by an embedder.
- **Worker = dedicated OS thread + current-thread Tokio runtime + `LocalSet`.**
  This is the only architecture where the `Send` handle works from any Tokio
  context (multi-thread runtime, current-thread runtime, or no runtime at all)
  and where the VM thread cannot be stolen by Tokio's work-stealing scheduler.
  Embedded-LocalSet variants ("run the worker on the user's thread") are
  rejected because they leak runtime setup back to every caller.
- **The worker boundary has a `Send` error type.** The existing `ruau::Error`
  is intentionally `!Send + !Sync` because it can contain VM handles, `Rc`
  chains, and non-`Send` external errors. Anything returned through
  `LuauWorkerHandle` must use a worker-specific `Send + Sync + 'static` error
  type that preserves useful text, categories, and source diagnostics without
  leaking local VM state.
- **Concurrent requests interleave on the VM lane.** Requests are
  `tokio::task::spawn_local`-ed on the worker's `LocalSet`, so async host
  callbacks can await Tokio resources concurrently with other in-flight
  requests. Strict per-request serialization is left to the embedder (e.g. by
  driving requests one-at-a-time on the handle side). This is cooperative
  concurrency, not atomic execution: request closures should not hold userdata
  guards, app-data borrows, or other exclusive VM-side borrows across `await`
  unless the embedder explicitly wants later requests to observe those borrow
  conflicts.
- **Sync vs async callback split (Stage Eight) is orthogonal to the worker.**
  Both modes use the same `create_function` / `create_async_function` registration.
- **No external backwards compatibility is required.** This crate has no
  published downstream consumers that need migration support; breaking
  changes are free, and no deprecation cycles, compatibility shims, or
  re-export aliases are needed. Audits in later stages cover in-repo
  callers (`examples/`, `tests/`, `benches/`, and internal modules) only.
  Changelog entries are for internal tracking, not user migration.

1. Stage One: Document The Runtime Contract

Make the public async story precise before changing behavior. The model should
separate direct local use from worker-handle use.

1. [ ] Update crate-level docs in `crates/ruau/src/lib.rs` to say that `ruau`
   is Tokio-based and that direct `Luau` handles produce local `!Send` futures.
2. [ ] Replace "or another local executor" wording in `function.rs`,
   `thread.rs`, `chunk.rs`, and README examples with the Tokio local-lane
   model.
3. [ ] Document two supported modes: direct local mode with `Luau`, and worker
   mode with a `Send` handle for multi-thread Tokio applications.
4. [ ] Standardize direct-mode examples on
   `#[tokio::main(flavor = "current_thread")]`. When an example needs
   `spawn_local`, follow the existing pattern of building a `LocalSet`
   explicitly inside `main` and driving the body with
   `LocalSet::new().run_until(...)`. Do not rely on a `flavor = "local"`
   macro attribute in examples; explicit `LocalSet` setup is portable across
   Tokio versions and keeps the local-task boundary visible to readers.
5. [ ] Add at least one multi-thread Tokio example that uses the worker handle
   from ordinary `tokio::spawn` tasks.
6. [ ] Document when `tokio::task::LocalSet` is required for direct `Luau`
   usage: spawning local VM futures or mixing `spawn_local` with Luau callbacks.
7. [ ] Add a short "Runtime model" section to README that explains why `Luau`
   is `!Send + !Sync`, why VM handles stay thread-affine, and how the worker
   handle integrates with normal multi-thread Tokio code.

2. Stage Two: Add An Idiomatic Tokio Worker API

Expose a first-class worker abstraction so multi-thread Tokio users do not have
to hand-roll `LocalSet`, `mpsc`, and `oneshot` plumbing.

1. [ ] Choose final names for the worker types. Prefer `LuauWorker` for the
   builder/owner and `LuauWorkerHandle` for the cloneable `Send` handle; avoid
   `Runtime` to prevent confusion with Tokio's runtime.
2. [ ] Spawn the worker on a dedicated OS thread that hosts a current-thread
   Tokio runtime and a `LocalSet`. The thread is the sole owner of the `Luau`
   VM. This makes the handle usable from any Tokio context (multi-thread,
   current-thread, or none) and isolates the VM from Tokio's work-stealing.
   Enable the Tokio time and I/O drivers on this runtime when the corresponding
   Tokio features are available, so async host callbacks can use normal Tokio
   resources from the worker lane.
3. [ ] Drive request dispatch by spawning each incoming request as
   `spawn_local` on the worker's `LocalSet` so requests interleave at await
   points; the VM remains single-threaded but async host callbacks do not
   block other in-flight requests.
4. [ ] Make the handle `Clone + Send + Sync` (typically wrapping
   `tokio::sync::mpsc::Sender`), while keeping `Luau`, `Table`, `Function`,
   `Thread`, `AnyUserData`, and other VM handles `!Send`.
5. [ ] Define `LuauWorkerError` / `LuauWorkerResult<T>` (names tentative) as
   the public handle error path. Do not expose `ruau::Result<T>` from worker
   methods, because `ruau::Error` is not `Send` and therefore cannot be the
   output of futures spawned with `tokio::spawn`. Sketch the variants up
   front so error handling is designed, not retrofitted:
   - `Vm { kind, message }` — runtime, syntax, memory, etc.; preserve the
     `ruau::Error` category as `kind` and the rendered text as `message`.
     Keep the original error in a non-`Send` `source` slot only if it can be
     converted to a `Send` representation first; otherwise drop the source.
   - `Conversion(String)` — `FromLuau` / `IntoLuau` failures at the boundary.
   - `Cancelled` — caller dropped the response future before completion.
   - `Shutdown` — channel closed, worker is no longer accepting requests.
   - `Panicked(String)` — VM lane task panicked; carries the panic payload's
     string form when available.
   - `JoinFailed(String)` — Tokio blocking-task join failure (from
     Stage Four file paths) surfaced through the worker.
6. [ ] Provide an async escape hatch such as
   `handle.with_async(|lua| Box::pin(async move { ... }))` that runs a local
   future on the VM lane and returns an owned `Send + 'static` result. The
   request closure is `FnOnce + Send + 'static`; the future it produces is
   `!Send` and may borrow `&Luau`, but borrowed VM handles cannot escape that
   future. Prototype the exact signature before committing the public name,
   because higher-ranked async closure ergonomics are easy to get wrong.
7. [ ] Optionally add a sync helper such as `handle.with(...)` for
   `FnOnce(&Luau) -> R` work. It is not sufficient as the only escape hatch,
   but it is useful for short VM operations and can be implemented on top of
   the async lane machinery.
8. [ ] Keep the worker boundary honest: values crossing into or out of the
   worker must be owned and `Send + 'static`, preferably serde-shaped data or
   plain conversion types.
9. [ ] Do not allow borrowed VM handles, userdata guards, tables, functions, or
   threads to escape the worker task. Compile-fail tests must cover this.
10. [ ] Define cancellation: implement it through the `JoinHandle::abort()`
    of the `spawn_local` task that runs each request. Because local task join
    handles may not be `Send`, keep them on the worker lane: assign each
    request an id, store pending/in-flight handles in worker-local state, and
    have the caller-side future own a `Send` drop guard that sends a cancel
    command back to the worker when dropped. Cancellation before spawn drops
    the pending request; cancellation after spawn calls `abort()`; cancellation
    after completion is ignored. Tokio drops the aborted future at the next
    poll boundary, which propagates `Drop` through async Luau callbacks and
    host futures. Currently executing synchronous Luau code runs to its next
    yield point; a hard interrupt remains the job of `Luau::set_interrupt`.
    Map aborted task joins to `LuauWorkerError::Cancelled` and panicking joins
    to `LuauWorkerError::Panicked`. A custom `CancellationToken` is unnecessary
    unless we ever need cooperative-but-not-aborting cancellation, which is out
    of scope for v1.
11. [ ] Define shutdown: dropping the last `LuauWorkerHandle` closes the channel
   and the lane drains in-flight tasks, drops the VM, and exits the thread.
   Provide `LuauWorker::shutdown()` (consumes the worker, closes request
   admission even if cloned handles still exist, then awaits drain) for ordered
   shutdown. After admission closes, new `handle.with` calls return
   `LuauWorkerError::Shutdown` (name tentative). Requests already accepted
   before shutdown either complete or cancel according to the normal
   cancellation rules; document which queued-but-not-spawned state counts as
   accepted.
12. [ ] Decide channel back-pressure: bounded `mpsc` with caller-configurable
    capacity and a documented default. `try_with` / `try_send`-style methods
    are out of scope for v1.
13. [ ] Add tests proving the handle can be cloned and used from multiple
    `tokio::spawn` tasks on a multi-thread runtime, including: dropped futures
    cancel cleanly, last-handle-drop drains, and an explicit `shutdown()`
    completes outstanding requests.

3. Stage Three: Provide Worker-Level Convenience Operations

The worker should be useful without requiring every caller to write a custom
closure for common embedding workflows.

1. [ ] Use a builder for setup: `LuauWorker::builder() -> LuauWorkerBuilder`
   with chainable methods covering the existing `Luau::new_with` surface and
   common embedder needs:
   - `.std_libs(StdLib)`
   - `.options(LuauOptions)`
   - `.compiler(Compiler)`
   - `.channel_capacity(usize)`
   - `.thread_name(impl Into<String>)`
   - `.with_setup(F)` where `F: FnOnce(&Luau) -> ruau::Result<()> + Send + 'static`
     — the universal escape hatch for VM init. Host registration (e.g.
     constructing a `HostApi` and calling `.install(&lua)`), resolver
     installation, app-data seeding, and any other VM-side wiring all live
     here. May be called more than once; closures run in registration order.
   Do not accept the current `HostApi` or `Rc<dyn ModuleResolver>` directly
   on the builder: today both surfaces are intentionally local (no `Send`
   bounds, `Rc`-friendly), so they cannot cross the thread boundary by
   value. Either users construct them inside `with_setup`, or a future
   iteration introduces typed parallel surfaces (`WorkerHostApi`,
   `WorkerResolver`) once usage data shows the typed version pays for itself.
   Avoid shipping factory variants alongside `with_setup` in v1 — they would
   be redundant. Setup errors are converted to `LuauWorkerError` before
   crossing back to the builder thread.
   `.build()` spawns the worker thread, runs setup to completion, and returns
   `LuauWorkerResult<LuauWorker>` so init failures surface synchronously to
   the caller.
   `worker.handle() -> LuauWorkerHandle` clones a `Send` handle.
2. [ ] Add convenience methods on `LuauWorkerHandle` that mirror the direct
   `Chunk` / `Function::call` surface but take owned `Send + 'static` inputs
   and return `Send + 'static` outputs through `FromLuau` / `IntoLuau`:
   - `exec(source) -> impl Future<Output = LuauWorkerResult<()>>`
   - `eval::<R>(source) -> impl Future<Output = LuauWorkerResult<R>>`
   - `call::<R>(global_name, args) -> impl Future<Output = LuauWorkerResult<R>>`
   Source is accepted as `impl AsChunk + Send + 'static` to keep parity with
   direct mode, but after Stage Nine this means in-memory chunks only
   (`String`, `&'static str`, `Vec<u8>`, and `chunk!` output), not paths.
   Inputs/outputs require `Send + 'static`; this rules out `Value`, `Table`,
   and other VM-borrowed handles at the boundary, which is the intended
   contract. Do not preserve path-backed loading through worker convenience
   methods; callers should load bytes with `tokio::fs::read(path).await?` and
   pass the resulting source, or a later explicit `exec_file`-style helper can
   be designed separately.
3. [ ] Treat checked loading as a worker convenience method that takes an
   owned `ResolverSnapshot` (already `Send`-capable as a pure data structure)
   and a configured `Checker`. Because requests crossing the handle are
   `'static`, the worker API cannot borrow `&mut Checker` from the caller.
   Either make the worker own a reusable checker configured at build time, or
   take ownership of a checker for one call and return it with the result.
   The raw `with_async` workflow stays available for embedders that want full
   control.
4. [ ] Support async host functions inside the worker transparently: any async
   host function registered through the worker-safe host setup path runs on the
   worker's `LocalSet` and can `await` arbitrary Tokio resources. Callers do
   not see the internal async poller.
5. [ ] Add a multi-thread Tokio example (e.g. an axum or hyper handler) that
   shares a `LuauWorkerHandle` across request tasks and uses a mix of typed
   convenience methods and `with(...)` closures.

4. Stage Four: Remove Blocking I/O From Async File Paths

Several file-backed APIs are currently async-shaped but perform blocking
filesystem probes or reads before reaching their first await point. With Tokio
mandatory, async execution paths should not block the runtime thread or the VM
worker lane.

1. [ ] Audit file-backed public APIs: `FilesystemResolver::resolve`,
   `AsChunk for &Path` / `PathBuf`, `Checker::check_path*`, and
   `Checker::add_definitions_path`.
2. [ ] Change `FilesystemResolver::resolve` in `crates/ruau/src/resolver.rs`
   to move path probing with `resolve_module_file` and the final source read
   into one helper executed through `tokio::task::spawn_blocking`.
3. [ ] Move `Checker::check_path_with_options` file loading through
   `spawn_blocking` before invoking the native checker.
4. [ ] Path-based `Luau::load(path)` is removed in Stage Nine (prune
   public API surface); this stage leaves the existing `AsChunk for &Path`
   / `PathBuf` impls in place so the cut lands as one diff. Examples that
   load from disk should provisionally migrate to
   `tokio::fs::read(path).await?` followed by `lua.load(bytes)`.
5. [ ] Document any remaining synchronous path APIs, such as
   `Checker::add_definitions_path`, as blocking setup helpers.
6. [ ] Preserve existing error behavior for missing, ambiguous, and unreadable
   modules or files.
7. [ ] Map Tokio blocking task join failures into `ModuleResolveError::Read`,
   `AnalysisError::ReadFile`, or another explicit error path.
8. [ ] Document the Tokio-runtime-context requirement: `spawn_blocking` panics
   when called outside a runtime, so file-backed async APIs require an active
   Tokio runtime. This holds for both direct mode (already true via the
   `Chunk` future) and worker mode (the worker thread has its own runtime).
9. [ ] Add or update tests proving filesystem resolver behavior remains the
   same for `.luau`, extension override, and `init.luau` modules.
10. [ ] Add or update tests covering path-backed checker loading and any final
    documented behavior for path-backed chunks.

5. Stage Five: Hide Async Poll Internals

The async poll sentinels are implementation details of the Luau coroutine
poller and should not be user-visible API.

1. [ ] Make `Luau::poll_pending()` `pub(crate)`. It is currently `pub` with
   `#[doc(hidden)]`, but a hidden `pub` is still callable from outside the
   crate. `poll_terminate()` and `poll_yield()` are already `pub(crate)`.
2. [ ] Keep all three markers together as private async-runtime implementation
   details in `state` or a smaller internal async module.
3. [ ] Verify no examples, tests, or public docs mention these sentinels
   (current `rg` already shows zero external references — re-verify after the
   visibility change).
4. [ ] Add compile-fail coverage if needed to ensure external code cannot name
   the internal poll marker.

6. Stage Six: Simplify Resolver Future Types

Keep `ModuleResolver` dyn-compatible, but stop exposing the full boxed future
spelling in every implementor.

1. [ ] Add a public alias such as `resolver::ResolveFuture<'a>` or
   `resolver::LocalResolveFuture<'a>` for the boxed local resolver future.
2. [ ] Change `ModuleResolver::resolve` to return that alias instead of
   spelling out `Pin<Box<dyn Future<...>>>`.
3. [ ] Update `ResolverSnapshot`, `InMemoryResolver`, `FilesystemResolver`, and
   the `Rc<T>` forwarding implementation to use the alias.
4. [ ] Keep the future non-`Send`, because resolvers run on the same local VM
   thread and may close over `Rc` or other `!Send` state.
5. [ ] Update rustdoc for `ModuleResolver` to explain why the future is local.
6. [ ] AFIT (`async fn resolve(...)`) was considered but rejected: it loses
   dyn compatibility, and `Rc<dyn ModuleResolver>` is part of the existing
   surface. Revisit only if dyn-async-fn-in-trait stabilises.

7. Stage Seven: Reframe Manual Coroutine APIs

Manual coroutine stepping remains useful, but it should read as an advanced VM
control rather than the default execution model.

1. [ ] Keep `Thread::resume`, `Thread::resume_error`, `Thread::status`, and
   `Thread::into_async` public if current tests or examples demonstrate real
   embedder use.
2. [ ] Move primary docs and examples toward `Chunk::exec`, `Chunk::eval`,
   `Chunk::call`, and `Function::call` as the canonical direct-mode execution
   APIs.
3. [ ] Move detailed coroutine-stepping examples under the `thread` module
   docs instead of crate-level getting-started material. The `vm` advanced
   module is unsettled (see Stage Nine item 6); avoid landing this content
   there until that decision lands.
4. [ ] Decide whether `Thread` and `AsyncThread` should remain root exports or
   be treated as advanced types surfaced primarily through a namespace.
5. [ ] Preserve `AsyncThread` stream behavior for yield iteration if it remains
   part of the public advanced API.

8. Stage Eight: Keep Sync And Async Callback Boundaries Explicit

Tokio-only does not mean every callback should become async. Preserve the cheap
sync path and use async only when a callback may suspend.

1. [ ] Keep `Luau::create_function` and `Luau::create_async_function` as
   separate APIs.
2. [ ] Keep `HostApi::global_function` and `HostApi::global_async_function` as
   separate registration helpers.
3. [ ] Keep `HostNamespace::function` and `HostNamespace::async_function`
   separate so host tables document suspension points clearly.
4. [ ] Keep sync and async userdata method registration separate unless a later
   implementation shows duplication without semantic value.
5. [ ] Add docs explaining that sync callbacks are preferred unless they need
   to await or yield.

9. Stage Nine: Prune Public API Surface

Reduce the crate's public footprint before shipping the worker. Each cut needs
an in-repo usage audit (`examples/`, `tests/`, `benches/`, internal modules)
before removal. Items move from low-risk mechanical fixes to higher-judgment
cuts. Treat this stage as separable from the worker implementation if any
high-judgment item starts blocking the Tokio API work.

1. [ ] Demote internal helpers that escaped via `pub use`. All of these are
   FFI-shaped and have no documented purpose outside the crate; move each
   to `pub(crate)` after confirming there are no `examples/`, `tests/`, or
   benchmark references:
   - `state::RawLuau` (`lib.rs:23` via `state/mod.rs:23`)
   - `state::ExtraData` (`lib.rs:22` via `state/mod.rs:22`) — currently
     re-exported as `pub` despite having zero public methods.
   - `state::util::callback_error_ext` (`state/mod.rs:24`)
   - `types::XRc` (`types/mod.rs:8`)
   - `types::ValueRef` and `types::ValueRefIndex` (`types/mod.rs:21`)
   - `state::LuauLiveGuard` (`state/mod.rs:66`)
2. [ ] Audit `WeakLuau` (`state/mod.rs:60`). It exists for circular-
   reference avoidance; if no in-repo example or test holds a weak VM
   handle, demote it to `pub(crate)` alongside `LuauLiveGuard`. Document
   the rationale either way.
3. [ ] Drop Roblox-flavored optimizer and feature knobs unless an in-repo
   call site relies on each. Each cut deletes the method, the supporting
   types, and any builder fields, FFI plumbing, or examples that exist
   only to feed it:
   - `Luau::set_fflag` (`state/mod.rs:851`).
   - `Luau::enable_jit` (`state/mod.rs:841`) — fold into `LuauOptions` if
     retained.
   - `Compiler::add_library_constant`, `add_vector_constant`,
     `add_disabled_builtin` and the `CompileConstant` enum,
     `library_constants` / `libraries_with_known_members` /
     `disabled_builtins` builder fields, and the
     `library_member_constant_callback` FFI shim in `Compiler::compile`.
   - `Luau::set_thread_callbacks` / `remove_thread_callbacks`
     (`state/mod.rs:607,665`) and the `ThreadCallbacks`,
     `ThreadCollectFn`, `ThreadCreateFn` types.
   - `Function::coverage` (`function.rs:363`), the `CoverageInfo` type,
     `Compiler::coverage_level`, the `CoverageLevel` enum, and
     `examples/coverage.rs`.
4. [ ] Tighten the surface under the Tokio-only banner:
   - Remove `AsChunk for &Path` and `AsChunk for PathBuf`
     (`chunk.rs:95-113`). Resolves Stage Four item 4. Update `examples/`,
     rustdoc, and the README to use `tokio::fs::read(path).await?` plus
     `lua.load(bytes)`.
   - Drop the `Either` re-export from the crate root (`lib.rs:194`) and
     from `types::mod.rs` (`types/mod.rs:19`). Users who need it depend on
     the `either` crate directly.
   - Drop `Luau::yield_with` (`state/mod.rs:1564`) only if the remaining
     async-callback and `AsyncThread` story still has a documented way to
     cover intended yield use cases. If it is the only Rust-side way to yield
     from an async host function, keep it as an advanced helper instead of
     cutting it under the Tokio cleanup banner.
   - Drop `Function::bind` (`function.rs:212`) and `Function::deep_clone`
     (`function.rs:422`). Both have script-side equivalents.
5. [ ] Audit `Luau::scope` plus the `Scope` API (`scope.rs`). The
   `examples/guided_tour.rs` walkthrough openly calls scope-bound
   callbacks "sketchy" and drives them through synchronous coroutine
   resumption that this plan otherwise demotes. If the only in-repo use
   is the guided-tour demo of the feature itself, drop the `scope`
   method, the `Scope` struct, and the non-`'static` callback machinery
   (and remove the demo). **Confirm with the user before cutting** —
   this is the highest-judgment item in the stage.
6. [ ] Once the items above land, decide the fate of the `vm` advanced
   module (`vm.rs`). If the surviving advanced exports are small enough,
   fold them into the crate root or their owning modules and delete
   `vm.rs`. If they still warrant a group, rename to `ruau::advanced` so
   the module name signals "off the main path" instead of competing with
   `vm` as a crate-level concept.

10. Stage Ten: Validation

Finish by proving the Tokio-only contract and the pruned surface are reflected
in docs, examples, and runtime behavior.

1. [ ] Run `rg -n "another local executor|executor-neutral"` over
   `README.md`, `examples`, and `crates/ruau/src` and resolve stale runtime
   wording.
2. [ ] Run `rg -n "std::fs::read|fs::read_to_string"` over
   `crates/ruau/src` and verify each remaining filesystem read is either
   inside a `spawn_blocking` helper or belongs to a documented synchronous
   setup/convenience API.
3. [ ] Run `rg -n "poll_pending|poll_terminate|poll_yield"` over `README.md`,
   `examples`, and public-facing rustdoc sections; resolve any public mentions.
4. [ ] Run focused resolver tests, including filesystem resolver tests.
5. [ ] Run async/thread tests that cover async callbacks, `Function::call`,
   `Chunk::call`, `Thread::into_async`, and yield iteration.
6. [ ] Run worker API tests on a multi-thread Tokio runtime, including multiple
   concurrent `tokio::spawn` callers and orderly shutdown.
7. [ ] Run docs or compile tests proving non-`Send` VM handles do not cross the
   worker boundary.
8. [ ] Verify worker cancellation: dropping the future returned by `with_async`
   / `with` / `exec` / `eval` cancels the in-flight task at the next await
   point and does not leak the request slot.
9. [ ] Verify worker shutdown semantics: drop-of-last-handle drains in-flight
   tasks and exits the worker thread; explicit `LuauWorker::shutdown()`
   completes outstanding requests before returning.
10. [ ] Verify async host functions registered through the worker-safe host
    setup path work transparently inside the worker, including those that
    await multi-thread Tokio resources (e.g. `tokio::sync::Mutex`, `reqwest`).
11. [ ] Capture the public API with `ruskel` before and after the prune, and
    diff the two snapshots for the changelog. The diff should match Stage
    Nine's intended cuts and contain no surprises.
12. [ ] Run the public-prune audit command below over `examples/`,
    `crates/ruau/tests/`, and `crates/ruau/benches/`; every hit must be a
    deliberate internal use or already removed.

    ```bash
    rg -n \
      -e "RawLuau|ExtraData|callback_error_ext|XRc|ValueRef|LuauLiveGuard" \
      -e "set_fflag|enable_jit|set_thread_callbacks|yield_with" \
      -e "Function::bind|deep_clone|Luau::scope" \
      examples/ crates/ruau/tests/ crates/ruau/benches/
    ```
13. [ ] Confirm `AsChunk for &Path` / `PathBuf` are removed and that no
    example or doctest still passes a path to `lua.load(...)`.
14. [ ] Run `cargo doc --no-deps` and resolve any broken intra-doc links
    introduced by the prune.
15. [ ] Run `cargo fmt --all`.
16. [ ] Run the project-standard test command.
