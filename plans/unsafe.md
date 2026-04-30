# Unsafe Surface Reduction

`ruau` is an FFI binding to Luau, so some unsafe is unavoidable: the C API is
inherently unsafe and the high-level crate wraps `*mut lua_State` and pointer
plumbing (`ExtraData`, `WrappedFailure`, ref-thread accounting). The goal of
this plan is **not** to eliminate unsafe. It is to make the unsafe surface
**auditable**: every unsafe site should have a clear scope, a documented
contract, and a small enough blast radius that a reviewer can convince
themselves it is sound.

The plan is staged so each stage is independently mergeable and produces a
measurable reduction in the unaudited surface, without committing to any later
stage.

## Current Baseline

Snapshot taken before any work begins. Numbers below are total occurrences in
`crates/ruau` and `crates/ruau-sys` source (excluding tests, examples, and
generated code).

- `crates/ruau-sys`: ~77 `pub unsafe fn` declarations. These mirror the Luau
  C ABI and are largely unavoidable.
- `crates/ruau`:
  - 36 `pub unsafe fn` (callable by external users).
  - 165 total `unsafe fn` declarations (mostly `pub(crate)` or private helpers).
  - ~210 `unsafe { ... }` blocks across the high-level crate.
  - 8 `unsafe impl Send`/`Sync` declarations.
  - 38 `unsafe extern "C"` / `extern "C-unwind"` callback functions.
  - 32 `SAFETY:` / `# Safety` comments — most clustered in `analyzer.rs`,
    very sparse in `state/`, `conversion.rs`, `table.rs`, and
    `userdata_impl/`.
- Workspace-wide `#![allow(unsafe_op_in_unsafe_fn)]` is set in both
  `crates/ruau/src/lib.rs` and `crates/ruau-sys/src/lib.rs`. This is the
  largest single multiplier on the unaudited surface: it disables per-operation
  scrutiny inside every `unsafe fn`, of which there are 165.

Hotspots (file, lines, unsafe blocks/declarations):

| File | Lines | `unsafe fn` | `unsafe { ... }` |
| --- | --- | --- | --- |
| `state/mod.rs` | 1752 | 4 | 55 |
| `state/raw.rs` | 1424 | 34 | 27 |
| `conversion.rs` | 1144 | 30 | 0 (all inside `unsafe fn`) |
| `table.rs` | 1327 | 1 | 27 |
| `multi.rs` | 532 | 15 | 0 |
| `analyzer.rs` | 2767 | 5 | 16 |
| `util/mod.rs` | 299 | 14 | 1 |
| `util/error.rs` | 408 | 7 | 0 |
| `util/userdata.rs` | 158 | 9 | 0 |
| `userdata_impl/util.rs` | 344 | 6 | 0 |
| `userdata_impl/mod.rs` | 1059 | 1 | 11 |

Recurring patterns:

1. **Stack discipline on `*mut lua_State`** — the bulk of unsafe in
   `state/raw.rs`, `table.rs`, `function.rs`, `conversion.rs`, and `multi.rs`.
   Every operation must be paired with `check_stack`, an optional `StackGuard`,
   and (for ops that can `longjmp`) the `protect_lua!` macro.
2. **`(*lua.extra.get()).field` access** — repeated ~49 times. `extra` is an
   `UnsafeCell<ExtraData>` and the project relies on the single-threaded VM
   invariant. There is no shared accessor.
3. **`unsafe extern "C-unwind"` callbacks** — 38 of these. The shape is
   stereotyped: a thin trampoline that calls `callback_error_ext`.
4. **FFI resource RAII** — the `RawGuard<T: FfiResource>` pattern in
   `analyzer.rs` is a clean abstraction that should be reused.
5. **`unsafe impl Send`/`Sync`** — 8 sites. Some have SAFETY comments
   (`analyzer.rs`), others (`userdata_impl/registry.rs`) do not.
6. **Public unsafe-fn helpers in `util/`** — `assert_stack`, `check_stack`,
   `push_string`, `push_table`, `rawset_field`, `to_string`, `ptr_to_str`,
   `get_userdata`, `take_userdata`, etc. All are `pub` only because they are
   used across crate-internal modules; none are part of the documented public
   API.

## Design Rules

1. Treat unsafe as a property of a *block*, not of a *function*. A function
   that internally needs unsafe but exposes a sound contract should be safe.
2. Every `unsafe { ... }` block, every `unsafe fn`, and every
   `unsafe impl` must have a `SAFETY:` comment that names the invariants
   being relied on.
3. The width of an `unsafe { ... }` block should be the smallest scope that
   can express the unsafe operation. Whole-method blocks are a smell.
4. The high-level crate should not export `unsafe fn` unless the safety
   contract is fundamental to the API (e.g. `Luau::load_bytecode`). Internal
   helpers should be `pub(crate)`.
5. `ruau-sys` is a different beast: it mirrors a C ABI and its `unsafe fn`
   surface is essentially pre-decided by the Luau headers. Audit effort there
   should focus on resource ownership, not on per-call rewriting.
6. Prefer typed RAII guards (`StackGuard`, `RawGuard<T: FfiResource>`,
   `StateGuard`) over manual cleanup. New patterns that recur should become
   guards.
7. Lints are the enforcement mechanism. Once a stage lands, its invariants
   should be *enforced by the compiler*, not by review.

## Stage One: Baseline And Lint Posture

Make the current surface measurable and stop it from silently growing. This
is the cheapest stage and unlocks every following stage.

1. [x] Add an `xtask unsafe-audit` subcommand that prints, for each crate:
   total `unsafe fn`, `unsafe { ... }`, `unsafe impl`, `pub unsafe fn`,
   `unsafe extern` callback count, and `SAFETY:` comment count. Output is
   plain text suitable for `diff` against a stored baseline.
2. [x] Commit the current baseline (the table from this plan, plus the raw
   numbers per file) to `crates/ruau/SAFETY.md` as an audit anchor.
3. [x] Wire `xtask unsafe-audit` into `cargo xtask tidy` with a soft check
   (warn on regression for now; convert to a hard check after Stage Three).
4. [x] Switch `unsafe_op_in_unsafe_fn` from `allow` to `warn` at the
   workspace level (do not yet remove the per-crate `#![allow(...)]`).
5. [x] Add `clippy::undocumented_unsafe_blocks = "warn"` and
   `clippy::missing_safety_doc = "warn"` to `[workspace.lints.clippy]`.
   Tolerated globally during this stage via crate-level allows that point
   at the stage which removes them.
6. [x] Decide on the policy for `ruau-sys`: keep
   `#![allow(unsafe_op_in_unsafe_fn)]` and
   `#![allow(clippy::missing_safety_doc)]` as a permanent file-level allow
   with a comment explaining why (raw FFI surface). Do not extend this
   allowance to `ruau`.

Exit criterion: `cargo xtask tidy` reports the baseline numbers, and any
future change can be reasoned about as a delta.

## Stage Two: Tighten Visibility

Most of the high-level crate's `pub unsafe fn` are crate-internal helpers
that became `pub` only because they are used across modules. Demote them.
This shrinks the externally-auditable surface dramatically without changing
behavior.

1. [ ] Audit the 36 `pub unsafe fn` in `ruau`. Classify each as:
   - **Genuine public unsafe API** (the contract is fundamental to the user).
     Expected list: `Luau::load_bytecode`. Document the safety contract
     precisely under `# Safety`.
   - **Cross-module helper** (used inside the crate, never named by users).
     Demote to `pub(crate) unsafe fn`.
2. [ ] Demote in `crates/ruau/src/util/mod.rs`: `assert_stack`,
   `check_stack`, `check_stack_for_values`, `push_string`, `push_buffer`,
   `push_table`, `rawget_field`, `rawset_field`, `get_main_state`,
   `to_string`, `get_metatable_ptr`, `ptr_to_str`, `ptr_to_lossy_str`.
3. [ ] Demote in `crates/ruau/src/util/userdata.rs`:
   `push_internal_userdata`, `get_internal_metatable`,
   `init_internal_metatable`, `get_internal_userdata`, `push_userdata`,
   `push_userdata_tagged_with_metatable`, `get_userdata`, `take_userdata`,
   `get_destructed_userdata_metatable`.
4. [ ] Demote in `crates/ruau/src/util/error.rs`: `pop_error`,
   `protect_lua_call`, `protect_lua_closure`, `error_traceback_thread`,
   `init_error_registry`.
5. [ ] Demote in `crates/ruau/src/state/util.rs`: `callback_error_ext`.
6. [ ] Demote in `crates/ruau/src/userdata_impl/util.rs`:
   `borrow_userdata_scoped`, `borrow_userdata_scoped_mut`,
   `init_userdata_metatable`.
7. [ ] Demote in `crates/ruau/src/state/raw.rs`: `RawLuau::push`,
   `RawLuau::pop`, `RawLuau::push_value`, `RawLuau::pop_value`. `RawLuau`
   itself is no longer publicly nameable (per `plans/structure.md`), so
   nothing breaks externally.
8. [ ] After demotion, re-run `xtask unsafe-audit`. Expect public-unsafe
   count to drop from 36 toward 1 (just `Luau::load_bytecode`).
9. [ ] Add a trybuild compile-fail test asserting that
   `ruau::util::push_string` (and one or two of the helpers above) cannot be
   named from outside the crate.

Exit criterion: every `pub unsafe fn` left in `ruau` has a documented
safety contract that is fundamental to the API.

## Stage Three: Narrow Unsafe Blocks

Switch from "the body is unsafe" to "this expression is unsafe". This is the
single largest improvement to readability and auditability — blocks become
small enough to comment, and operations that turn out to be safe stop being
treated as unsafe.

1. [ ] Remove `#![allow(unsafe_op_in_unsafe_fn)]` from
   `crates/ruau/src/lib.rs`. The lint is already warning at workspace level
   from Stage One; removing the file-level allow makes it apply.
2. [ ] Sweep the resulting warnings module-by-module. For each `unsafe fn`,
   replace the implicit unsafe permission with explicit `unsafe { ... }`
   blocks at the smallest scope.
3. [ ] During the sweep, identify operations that turn out to be safe (most
   `Cell` reads, integer casts, plain field accesses, calls to other
   non-unsafe functions). Move them out of the unsafe block.
4. [ ] In `state/mod.rs`, the dominant pattern is methods whose body is one
   giant `unsafe { ... }`. Replace with narrow blocks. Examples to fix
   first: `Luau::sandbox`, `Luau::set_interrupt`, `Luau::scoped_interrupt`,
   `Luau::remove_interrupt`, `Luau::set_thread_callbacks`,
   `Luau::remove_thread_callbacks`, `Luau::traceback`, `Luau::inspect_stack`,
   `Luau::used_memory`, `Luau::set_memory_limit`, `Luau::gc_collect`.
5. [ ] In `state/raw.rs`, narrow blocks inside `init_from_ptr`, `new`,
   `load_chunk`, `create_string`, `create_buffer_with_capacity`,
   `create_table_with_capacity`, `create_table_from`,
   `create_sequence_from`, `create_thread`.
6. [ ] In `table.rs`, `function.rs`, `thread.rs`, narrow blocks per method.
   These methods are individually small; the goal is to make each unsafe
   block name a single FFI call or a single dereference.
7. [ ] After sweep, flip
   `clippy::undocumented_unsafe_blocks` from `warn` to `deny` in the
   workspace lints. Every remaining `unsafe { ... }` and every `unsafe fn`
   must have a `SAFETY:` comment. Re-running the build forces every
   site to be commented.
8. [ ] Convert `xtask unsafe-audit` from a soft check to a hard CI gate.

Exit criterion: no `unsafe fn` in `ruau` relies on
`unsafe_op_in_unsafe_fn`; every `unsafe { ... }` block is narrow and
documented.

## Stage Four: Extract Safe Wrappers Around Recurring Patterns

Several patterns repeat hundreds of times and are the largest contributors to
unsafe count. Wrap them once and the call sites become safe.

1. [ ] Replace direct `(*lua.extra.get())` access with an inherent method
   on `RawLuau`:
   ```rust
   /// Borrows the per-state extra data.
   ///
   /// # Safety
   /// Caller must not have an outstanding `extra_mut` borrow live; the
   /// VM invariant guarantees no concurrent access from another thread.
   pub(crate) unsafe fn extra(&self) -> &ExtraData { &*self.extra.get() }
   pub(crate) unsafe fn extra_mut(&self) -> &mut ExtraData { &mut *self.extra.get() }
   ```
   Then convert call sites; many will reduce to `lua.extra().field` inside
   a single narrow unsafe block instead of a re-derived deref.
2. [ ] Consider whether the borrow can be made entirely safe by tracking
   borrow state with `RefCell` (or `Cell<bool>`-checked at debug builds).
   The constraint to verify: callbacks invoked through Luau may re-enter
   methods that touch `extra`. If they do, leave the unsafe accessor and
   document the re-entrancy invariant; otherwise convert to `RefCell` and
   make the accessor safe.
3. [ ] Introduce `RawLuau::scoped_op<R>(&self, slots: c_int, f: impl
   FnOnce(*mut lua_State) -> Result<R>) -> Result<R>` that performs
   `StackGuard::new` + `check_stack` + the closure + guard drop in one
   place. Convert the dozens of `_sg = StackGuard::new(state); check_stack(state, N)?; ...` sequences in `state/mod.rs`,
   `state/raw.rs`, `table.rs`, `function.rs` to call it.
4. [ ] Generalise `analyzer::RawGuard<T: FfiResource>` into a public
   crate-internal `util::ffi::RawGuard` and apply it to other shim-allocated
   resources outside the analyzer (any new ones added later).
5. [ ] Make the `unsafe extern "C-unwind"` trampolines uniform: extract a
   `callback!(|extra, nargs| { ... })` macro that emits the trampoline
   skeleton with `callback_error_ext` already invoked. Cuts boilerplate in
   `state/mod.rs` and `state/raw.rs` and removes incidental unsafe.
6. [ ] After extraction, re-run `xtask unsafe-audit` and verify the
   `unsafe { ... }` block count drops materially in the patched modules.

Exit criterion: the four patterns above (`extra` access, scoped stack op,
FFI RAII, callback trampoline) each have a single canonical wrapper used
crate-wide.

## Stage Five: Document SAFETY At Every Site

After narrowing and wrapping, every remaining unsafe site is intrinsic. Pin
down its contract.

1. [ ] For every remaining `unsafe { ... }` block, add a `SAFETY:` comment
   that names the invariants being relied on. `clippy::undocumented_unsafe_blocks`
   (denied at the end of Stage Three) makes this enforced going forward; this
   stage is the one-time cleanup of any pre-existing exceptions.
2. [ ] For every `unsafe fn`, add a `# Safety` rustdoc section listing
   exactly what the caller must guarantee. `clippy::missing_safety_doc`
   enforces this once enabled (Stage One) — flip it from `warn` to `deny`
   here.
3. [ ] Audit the 8 `unsafe impl Send` / `unsafe impl Sync` declarations.
   Add a SAFETY comment to each; current omissions are in
   `userdata_impl/registry.rs` (`UserDataType`, `UserDataProxy<T>`).
4. [ ] Write a short top-of-file safety contract for each module that
   contains material unsafe: `state/raw.rs`, `state/mod.rs`, `util/error.rs`,
   `userdata_impl/cell.rs`, `userdata_impl/util.rs`, `analyzer.rs`. The
   per-module contract names the global invariants the file relies on
   (single-threaded VM, ref-thread accounting, callback re-entrancy rules)
   so individual `SAFETY:` comments can reference them by name instead of
   repeating them.
5. [ ] Consolidate the global invariants into `crates/ruau/SAFETY.md`:
   - VM is `!Send + !Sync` and pinned to one thread for its lifetime.
   - `extra` (`UnsafeCell<ExtraData>`) access rules.
   - Stack discipline (`StackGuard`, `check_stack`, `protect_lua!`).
   - `WrappedFailure` preallocation contract (`callback_error_ext`).
   - Resource ownership for shim-allocated FFI values.
   - `Send`/`Sync` exceptions.

Exit criterion: every unsafe site (block, fn, impl) is commented; module
and crate-level invariants are documented in one place; lints enforce.

## Stage Six: Isolate The FFI Boundary

Push remaining raw `*mut lua_State` plumbing entirely below the `RawLuau`
boundary. Higher-level types should call `RawLuau` methods that have already
absorbed stack discipline, so most of their methods become safe.

1. [ ] Audit `IntoLuau` / `FromLuau` / `IntoLuauMulti` / `FromLuauMulti`
   external implementer hooks (`push_into_stack`, `from_stack`, etc.).
   These hooks are `unsafe fn` on a public trait, which forces every
   external implementor onto the unsafe surface.
2. [ ] Move the stack-level hooks off the public traits and onto a sealed
   crate-private extension trait (e.g. `IntoLuauStack`, `FromLuauStack`).
   External implementors implement only the safe `into_luau` / `from_luau`
   methods; internal types implement the sealed trait for the fast path.
   This collapses the 30 `unsafe fn` impls in `conversion.rs` and 15 in
   `multi.rs` to crate-private.
3. [ ] Where `RawLuau::*` methods are still surfaced as `unsafe fn` to
   `Table`, `Function`, `Thread`, `LuauString`, etc., replace the unsafe
   contracts with safe wrappers that internalise the stack-discipline
   guarantee (using `scoped_op` from Stage Four).
4. [ ] Make `crates/ruau/src/util/userdata.rs` and the helper layer
   `pub(crate)` only — completed in Stage Two — and verify that no
   downstream module pierces the wrapper.
5. [ ] Apply `#![forbid(unsafe_code)]` per-module to files that, after the
   above, no longer need any unsafe at all. Candidates to verify:
   `runtime/heap_dump.rs`, `runtime/globals.rs`, `serde/mod.rs`,
   `types/registry_key.rs`, `traits.rs` (after the unsafe trait hooks
   move to the sealed extension), `resolver.rs`, `chunk.rs`, `stdlib.rs`.

Exit criterion: `IntoLuau` / `FromLuau` external implementors can be written
without `unsafe`; several modules forbid unsafe entirely.

## Stage Seven: Validation And Tooling

Lock in the gains.

1. [ ] Add `cargo miri test` to the test matrix for the test set that does
   not require FFI execution (Miri cannot run Luau itself). Document the
   subset that runs under Miri in `crates/ruau/SAFETY.md`.
2. [ ] Convert `xtask unsafe-audit` into a hard regression gate: it stores
   the baseline counts in a checked-in JSON file and fails if any number
   grows without an explicit baseline update.
3. [ ] Add a `cargo-geiger` (or equivalent) report to a CI job; do not
   gate on it, but publish the report so reviewers can see unsafe density
   per file at a glance.
4. [ ] Document an unsafe-review checklist in `crates/ruau/SAFETY.md`:
   for each new `unsafe { ... }` block, the reviewer must verify the
   SAFETY comment, the named invariant, the matching wrapper (if any),
   and the audit-baseline delta.
5. [ ] Final pass: run `cargo xtask tidy`, `cargo xtask test`,
   `cargo doc -p ruau --no-deps --all-features`, and `cargo miri test`
   (subset) before declaring this plan complete.

Exit criterion: unsafe surface size is monitored mechanically and cannot
regress silently.

## Non-Goals

- Eliminating unsafe from `ruau-sys`. It is an FFI binding crate.
- Eliminating `unsafe extern "C-unwind"` callbacks. They are required by
  the Luau C API.
- Replacing the `(*extra.get()).field` pattern with a fully-safe abstraction
  if doing so requires refactoring the entire callback re-entrancy model.
  Document and gate the unsafe accessor instead.
- Rewriting the `WrappedFailure` / `callback_error_ext` machinery. It is
  intricate but well-tested. Audit the SAFETY documentation here, do not
  redesign.
