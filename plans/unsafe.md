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

## Current State

Stage One landed in commit `14a2b89`. Its outputs are now part of the
workflow:

- [`crates/ruau/SAFETY.md`](../crates/ruau/SAFETY.md) — the live audit
  anchor: per-crate counts, per-file hotspots, lint policy, and audit
  policy. **This file is the source of truth for current numbers; this
  plan does not duplicate them.**
- `crates/ruau/audit-baseline.json` — machine-readable baseline used by
  `cargo xtask unsafe-audit` for soft regression checks.
- `cargo xtask unsafe-audit` (with `--verbose`, `--update-baseline`)
  prints the table; `cargo xtask tidy` runs the audit as a soft check.
- Workspace lints are at `warn`: `unsafe_op_in_unsafe_fn`,
  `clippy::undocumented_unsafe_blocks`, `clippy::missing_safety_doc`.
  Crate-level `#![allow(...)]` on `ruau` (transitional) and `ruau-sys`
  (permanent FFI exception) suppress until later stages clean up.

Recurring patterns identified during Stage One:

1. **Stack discipline on `*mut lua_State`** — bulk of unsafe in
   `state/raw.rs`, `table.rs`, `function.rs`, `conversion.rs`, `multi.rs`.
   Every operation pairs with `check_stack`, an optional `StackGuard`,
   and (for ops that can `longjmp`) the `protect_lua!` macro.
2. **`(*lua.extra.get()).field` access** — repeats ~49 times. `extra`
   is an `UnsafeCell<ExtraData>` and the project relies on the
   single-threaded VM invariant. There is no shared accessor.
3. **`unsafe extern "C-unwind"` callbacks** — 31 in `ruau`. The shape
   is stereotyped: a thin trampoline that calls `callback_error_ext`.
4. **FFI resource RAII** — the `RawGuard<T: FfiResource>` pattern in
   `analyzer.rs` is a clean abstraction that should be reused.
5. **`unsafe impl Send`/`Sync`** — 8 sites in `ruau`. Some have SAFETY
   comments (`analyzer.rs`), others (`userdata_impl/registry.rs`) do
   not.
6. **Public unsafe-fn helpers in `util/`** — `assert_stack`,
   `check_stack`, `push_string`, `push_table`, `rawset_field`,
   `to_string`, `ptr_to_str`, `get_userdata`, `take_userdata`, etc.
   All `pub` only because they are used across crate-internal modules;
   none are part of the documented public API.

## Design Rules

1. Treat unsafe as a property of a *block*, not of a *function*. A
   function that internally needs unsafe but exposes a sound contract
   should be safe.
2. Every `unsafe { ... }` block, every `unsafe fn`, and every
   `unsafe impl` must have a `SAFETY:` comment that names the
   invariants being relied on.
3. The width of an `unsafe { ... }` block should be the smallest scope
   that can express the unsafe operation. Whole-method blocks are a
   smell.
4. The high-level crate should not export `unsafe fn` unless the
   safety contract is fundamental to the API (e.g.
   `Luau::load_bytecode`). Internal helpers should be `pub(crate)`.
5. `ruau-sys` is a different beast: it mirrors a C ABI and its
   `unsafe fn` surface is essentially pre-decided by the Luau headers.
   Audit effort there should focus on resource ownership, not on
   per-call rewriting.
6. Prefer typed RAII guards (`StackGuard`, `RawGuard<T: FfiResource>`,
   `StateGuard`) over manual cleanup. New patterns that recur should
   become guards.
7. Lints are the enforcement mechanism. Once a stage lands, its
   invariants should be *enforced by the compiler*, not by review.
8. Test code (`crates/ruau/tests/*`) does not inherit the crate-level
   transitional allows. Any `unsafe { ... }` block added in tests
   needs a `SAFETY:` comment from day one.

## Sequencing Notes

The order of stages below is deliberate:

- **Wrappers before narrowing.** Stage Three introduces helpers that
  collapse whole categories of unsafe blocks. Doing the per-module
  narrow sweep (Stage Four) afterward means the sweep operates on
  fewer, simpler sites.
- **Narrow + document together.** The original plan split these into
  two passes per module. Folded into one: when you narrow a module,
  you also `SAFETY:`-comment it, and remove its allow exemption in the
  same change.
- **Trait sealing comes after the sweep.** Sealing the
  `IntoLuau`/`FromLuau` stack hooks is an API-level change. It does
  not reduce raw unsafe count (the override impls stay; their
  visibility changes), so it is independent of the sweep and benefits
  from already-narrow target blocks.
- **Lockdown last.** Hard-gating the audit, Miri runs, and
  `forbid(unsafe_code)` per-module only make sense after the work
  they would gate is done.

## Stage Two: Tighten Visibility

Most of the high-level crate's `pub unsafe fn` are crate-internal
helpers that became `pub` only because they are used across modules.
Demote them. This shrinks the externally-auditable surface dramatically
without changing behavior.

1. [x] Audit the 36 `pub unsafe fn` in `ruau`. Classify each as:
   - **Genuine public unsafe API** (the contract is fundamental to the
     user). Expected list: `Luau::load_bytecode`. Document the safety
     contract precisely under `# Safety`.
   - **Cross-module helper** (used inside the crate, never named by
     users). Demote to `pub(crate) unsafe fn`.
2. [x] Demote in `crates/ruau/src/util/mod.rs`: `assert_stack`,
   `check_stack`, `check_stack_for_values`, `push_string`,
   `push_buffer`, `push_table`, `rawget_field`, `rawset_field`,
   `get_main_state`, `to_string`, `get_metatable_ptr`, `ptr_to_str`,
   `ptr_to_lossy_str`.
3. [x] Demote in `crates/ruau/src/util/userdata.rs`:
   `push_internal_userdata`, `get_internal_metatable`,
   `init_internal_metatable`, `get_internal_userdata`, `push_userdata`,
   `push_userdata_tagged_with_metatable`, `get_userdata`,
   `take_userdata`, `get_destructed_userdata_metatable`.
4. [x] Demote in `crates/ruau/src/util/error.rs`: `pop_error`,
   `protect_lua_call`, `protect_lua_closure`,
   `error_traceback_thread`, `init_error_registry`.
5. [x] Demote in `crates/ruau/src/state/util.rs`: `callback_error_ext`.
6. [x] Demote in `crates/ruau/src/userdata_impl/util.rs`:
   `borrow_userdata_scoped`, `borrow_userdata_scoped_mut`,
   `init_userdata_metatable`.
7. [x] Demote in `crates/ruau/src/state/raw.rs`: `RawLuau::push`,
   `RawLuau::pop`, `RawLuau::push_value`, `RawLuau::pop_value`.
   `RawLuau` itself is no longer publicly nameable (per
   `plans/structure.md`), so nothing breaks externally.
8. [x] Add a trybuild compile-fail test asserting that
   `ruau::util::push_string` (and one or two of the helpers above)
   cannot be named from outside the crate.
9. [x] Re-run `cargo xtask unsafe-audit --update-baseline` and update
   `crates/ruau/SAFETY.md`. Expect the `pub unsafe fn` count to drop
   from 36 toward 1.

Exit criterion: every `pub unsafe fn` left in `ruau` has a documented
safety contract that is fundamental to the API.

## Stage Three: Extract Safe Wrappers Around Recurring Patterns

The four recurring patterns identified above repeat dozens to hundreds
of times each. Wrap them once and the call sites become safe (or, at
worst, narrowly unsafe at the wrapper-application site rather than
inline at every use).

1. [x] Replace direct `(*lua.extra.get())` access with an inherent
   method on `RawLuau`:
   ```rust
   /// Borrows the per-state extra data.
   ///
   /// # Safety
   /// Caller must not have an outstanding `extra_mut` borrow live; the
   /// VM invariant guarantees no concurrent access from another thread.
   pub(crate) unsafe fn extra(&self) -> &ExtraData { &*self.extra.get() }
   pub(crate) unsafe fn extra_mut(&self) -> &mut ExtraData { &mut *self.extra.get() }
   ```
   Convert the ~49 call sites; many reduce to `lua.extra().field`
   inside one narrow unsafe block instead of a re-derived deref.
2. [x] Investigate whether the borrow can be made entirely safe by
   tracking borrow state with `RefCell` (or a `Cell<bool>`-checked
   debug-only borrow tracker). The constraint to verify: callbacks
   invoked through Luau may re-enter methods that touch `extra`. If
   they do, leave the unsafe accessor and document the re-entrancy
   invariant; otherwise convert to `RefCell` and make the accessor
   safe.
3. [x] Introduce
   ```rust
   pub(crate) fn scoped_op<R>(
       &self,
       slots: c_int,
       f: impl FnOnce(*mut lua_State) -> Result<R>,
   ) -> Result<R>
   ```
   on `RawLuau` that performs `StackGuard::new` + `check_stack` + the
   closure + guard drop in one place. The closure body is still
   unsafe internally, but the wrapper signature is safe and the
   guard/check pair stops repeating. Convert the dozens of
   `_sg = StackGuard::new(state); check_stack(state, N)?; ...`
   sequences in `state/mod.rs`, `state/raw.rs`, `table.rs`,
   `function.rs`.
4. [x] Generalise `analyzer::RawGuard<T: FfiResource>` into a
   crate-internal `util::ffi::RawGuard` and apply it to other
   shim-allocated resources outside the analyzer. New shim resources
   added later use it by default.
5. [x] Make the `unsafe extern "C-unwind"` trampolines uniform. Extract
   a `callback!(|extra, nargs| { ... })` macro that emits the
   trampoline skeleton with `callback_error_ext` already invoked.
   Cuts boilerplate in `state/mod.rs` and `state/raw.rs` and removes
   incidental unsafe inside trampoline bodies.
6. [x] Re-run `cargo xtask unsafe-audit` and verify the
   `unsafe { ... }` block count drops materially in the patched
   modules. Update `crates/ruau/SAFETY.md` baseline.

Exit criterion: each of the four patterns above has a single canonical
wrapper used crate-wide, and the audit shows a measurable drop in
`unsafe { ... }` blocks.

### Stage Three Implementation Notes

What actually landed differs from the original wishlist in two places:

- **`scoped_op` adoption is opportunistic, not exhaustive.** The helper
  is in place and used as a worked example in `Table::set_protected`,
  but the broader rollout across `state/mod.rs`, `state/raw.rs`,
  `table.rs`, and `function.rs` was deferred. The audit metric does not
  improve from `scoped_op` adoption — the unsafe block per call site
  just moves from outside the helper to inside the closure. Stage
  Four's per-module narrowing pass is the right time to apply
  `scoped_op` site-by-site as those modules are touched.
- **The `callback!` trampoline macro was skipped.** Only three
  trampolines (`get_future_callback`, `poll_future`, `call_callback`)
  fit the exact `callback_error_ext` + `lua_upvalueindex(1)` pattern,
  and one of them (`poll_future`) extracts at a positional arg
  instead. The macro complexity outweighed the savings. Stage Four
  documents the trampolines per-site as part of the per-module sweep.

The "measurable drop in `unsafe { ... }` blocks" exit criterion is
relaxed: Stage Three increased the absolute count by a handful (helpers
have their own narrow unsafe bodies), but raised `SAFETY:` density by
21 comments and consolidated three of the four recurring patterns
behind named wrappers. The block-count reduction lands in Stage Four.

## Stage Four: Per-Module Narrow & Document Sweep

The dominant pattern in `state/mod.rs`, `state/raw.rs`, `table.rs`,
and the userdata layer is methods whose body is one giant
`unsafe { ... }`. Switch from "the body is unsafe" to "this expression
is unsafe", and document each remaining site as you go.

This stage is intentionally split into **multiple commits**, one per
module (or one per logical chunk of a large module). Trying to land it
in a single change has two failure modes: the diff becomes unreviewable
(~233 unsafe blocks across 10+ files), and the time investment crowds
out validation. Each module sweep is small enough to review in
isolation, and the audit baseline tracks cumulative progress.

The sweep is module-by-module. Each pass is a self-contained commit
that:

1. Lifts the relevant crate-level `#![allow]` for that module's lint
   warnings (initially via `#[allow(...)]` on the module item; the
   crate-level allows go away when the last user is gone).
2. Narrows every whole-method `unsafe { ... }` to the smallest scope.
3. Identifies operations that turn out to be safe (most `Cell` reads,
   integer casts, plain field accesses, calls to other non-unsafe
   functions) and moves them out of the unsafe block.
4. Adds a `SAFETY:` comment to every remaining `unsafe { ... }` block.
5. Adds a `# Safety` rustdoc section to every `unsafe fn` in the
   module that doesn't already have one.
6. Adds a top-of-file safety contract for the module if it relies on
   any of the global invariants (single-threaded VM, ref-thread
   accounting, callback re-entrancy rules) so individual `SAFETY:`
   comments can reference them by name.
7. Re-runs `cargo xtask unsafe-audit --update-baseline`.

Sweep order (largest hotspot first; numbers from the Stage One
baseline):

1. [x] `crates/ruau/src/state/mod.rs` (65 blocks). Two passes covered
   the file: pass 1 converted `Registry::*` and `create_c_function` to
   `scoped_op`; pass 2 added SAFETY comments to the interrupt /
   thread-callback machinery, GC + memory paths, the create_* family,
   app_data accessors, the Drop impls, the `WeakLuau` / `LuauLiveGuard`
   accessors, and the `# Safety` rustdoc on `interrupt_proc`,
   `userthread_proc`, `raw_luau`, and `run_thread_collection_callback`.
2. [x] `crates/ruau/src/table.rs` (29 blocks). All `Table::*` methods
   converted to `scoped_op` where the `StackGuard + check_stack +
   protect_lua` pattern fits; remaining inline unsafe sites
   (`metatable`, `set_metatable`, `set_readonly`, `set_safeenv`,
   `is_empty`, `is_readonly`, `has_metatable`, `clear`,
   `has_array_metatable`, `find_array_len`, `raw_seti`, `for_each`,
   `for_each_value_by_len`, the slice-equality impl, `TablePairs::next`,
   `TableSequence::next`) carry SAFETY comments naming the invariants.
3. [ ] `crates/ruau/src/state/raw.rs` (24 blocks). Examples:
   `init_from_ptr`, `new`, `load_chunk`, `create_string`,
   `create_buffer_with_capacity`, `create_table_with_capacity`,
   `create_table_from`, `create_sequence_from`, `create_thread`.
4. [ ] `crates/ruau/src/analyzer.rs` (17 blocks; already
   well-commented — verify completeness).
5. [ ] `crates/ruau/src/userdata_impl/mod.rs` (13 blocks).
6. [ ] `crates/ruau/src/thread.rs` (9 blocks).
7. [ ] `crates/ruau/src/userdata_impl/registry.rs` (8 blocks; also
   covers Send/Sync impl SAFETY for `UserDataType` and
   `UserDataProxy<T>`).
8. [ ] `crates/ruau/src/function.rs` (8 blocks).
9. [ ] `crates/ruau/src/scope.rs`, `crates/ruau/src/userdata_impl/cell.rs`,
   `crates/ruau/src/userdata_impl/ref.rs`, `crates/ruau/src/string.rs`,
   `crates/ruau/src/debug/stack.rs`, and remaining smaller modules.
10. [ ] After the last module: remove
    `#![allow(unsafe_op_in_unsafe_fn)]` and
    `#![allow(clippy::undocumented_unsafe_blocks)]` from
    `crates/ruau/src/lib.rs`.
11. [ ] Flip `unsafe_op_in_unsafe_fn` and
    `clippy::undocumented_unsafe_blocks` from `warn` to `deny` at the
    workspace level.

Exit criterion: no `unsafe fn` in `ruau` relies on
`unsafe_op_in_unsafe_fn`; every `unsafe { ... }` block is narrow and
documented; `crates/ruau/SAFETY.md` baseline reflects the new shape.

## Stage Five: Cross-Module Documentation & Send/Sync Audit

The per-module sweep documents per-block invariants. This stage
collects the cross-cutting documentation work.

1. [ ] Audit the 8 `unsafe impl Send` / `unsafe impl Sync`
   declarations. Add a SAFETY comment to each. Current omissions are
   in `userdata_impl/registry.rs` (`UserDataType`,
   `UserDataProxy<T>`); the sweep in Stage Four covers that file but
   the cross-cut audit ensures consistency across all eight sites.
2. [ ] Consolidate the global invariants into
   `crates/ruau/SAFETY.md` under a new "Global Invariants" section:
   - VM is `!Send + !Sync` and pinned to one thread for its lifetime.
   - `extra` (`UnsafeCell<ExtraData>`) access rules.
   - Stack discipline (`StackGuard`, `check_stack`, `protect_lua!`,
     `scoped_op`).
   - `WrappedFailure` preallocation contract (`callback_error_ext`).
   - Resource ownership for shim-allocated FFI values
     (`util::ffi::RawGuard`).
   - `Send` / `Sync` exceptions and their justifications.
3. [ ] Remove `#![allow(clippy::missing_safety_doc)]` from
   `crates/ruau/src/lib.rs`. Flip the lint from `warn` to `deny` at
   the workspace level. Every public-or-crate-private `unsafe fn` now
   has a `# Safety` section.
4. [ ] Final pass on Stage Two work: confirm every `pub unsafe fn`
   that survived demotion has a `# Safety` section that meets the
   new lint.

Exit criterion: every unsafe site (block, fn, impl) in `ruau` is
commented; module-level and crate-level invariants are documented in
one place; lints enforce.

## Stage Six: Seal Trait Stack Hooks

The `IntoLuau` / `FromLuau` / `IntoLuauMulti` / `FromLuauMulti`
external implementer hooks (`push_into_stack`, `from_stack`, etc.)
are `unsafe fn` on a public trait, which forces every external
implementor onto the unsafe surface. Sealing the hooks is a public-API
change separate from the rest of the cleanup.

1. [ ] Audit which hook overrides are *necessary* for performance
   versus which can fall back to the safe `into_luau` / `from_luau`
   default. Document the verdict.
2. [ ] Move the stack-level hooks off the public traits and onto
   sealed crate-private extension traits (e.g. `IntoLuauStack`,
   `FromLuauStack`). External implementors implement only the safe
   `into_luau` / `from_luau` methods; internal types implement the
   sealed trait for the fast path. This collapses the 30 `unsafe fn`
   impls in `conversion.rs` and 15 in `multi.rs` to crate-private.
3. [ ] Update rustdoc on `IntoLuau` / `FromLuau` to describe what an
   external implementor needs to know — and what they no longer need
   to know.
4. [ ] Where `RawLuau::*` methods are still surfaced as `unsafe fn`
   to `Table`, `Function`, `Thread`, `LuauString`, etc., replace the
   unsafe contracts with safe wrappers that internalise the
   stack-discipline guarantee (using `scoped_op` from Stage Three).
5. [ ] Re-run `cargo xtask unsafe-audit --update-baseline`. The
   `unsafe fn` count should drop materially in `conversion.rs` and
   `multi.rs`.

Exit criterion: external implementors of `IntoLuau` / `FromLuau` do
not need to write `unsafe`; `conversion.rs` and `multi.rs` no longer
declare unsafe fns at all.

## Stage Seven: Lockdown — Validation & Tooling

Lock in the gains.

1. [ ] Apply `#![forbid(unsafe_code)]` per-module to files that, after
   all earlier stages, no longer need any unsafe at all. Candidates
   to verify: `runtime/heap_dump.rs`, `runtime/globals.rs`,
   `serde/mod.rs`, `types/registry_key.rs`, `traits.rs` (after the
   trait seal), `resolver.rs`, `chunk.rs`, `stdlib.rs`. The build
   itself becomes the proof that these modules are unsafe-free.
2. [ ] Convert `cargo xtask unsafe-audit` from a soft check to a
   hard regression gate: it fails (non-zero exit) if any baseline
   number grows without an explicit `--update-baseline` commit. Wire
   into CI.
3. [ ] Add `cargo miri test` to the test matrix for the test set that
   does not require FFI execution (Miri cannot run Luau itself).
   Document the Miri-runnable subset in `crates/ruau/SAFETY.md`.
4. [ ] Add a `cargo-geiger` (or equivalent) report to a CI job; do
   not gate on it, but publish the report so reviewers can see
   unsafe density per file at a glance.
5. [ ] Document an unsafe-review checklist in
   `crates/ruau/SAFETY.md`: for each new `unsafe { ... }` block, the
   reviewer must verify the SAFETY comment, the named invariant,
   the matching wrapper (if any), and the audit-baseline delta.
6. [ ] Final pass: run `cargo xtask tidy`, `cargo xtask test`,
   `cargo doc -p ruau --no-deps --all-features`, and
   `cargo miri test` (subset) before declaring this plan complete.

Exit criterion: the unsafe surface is monitored mechanically and
cannot regress silently; clean modules cannot accidentally regain
unsafe; Miri exercises everything Miri can reach.

## Non-Goals

- Eliminating unsafe from `ruau-sys`. It is an FFI binding crate.
- Eliminating `unsafe extern "C-unwind"` callbacks. They are required
  by the Luau C API.
- Replacing the `(*extra.get()).field` pattern with a fully-safe
  abstraction if doing so requires refactoring the entire callback
  re-entrancy model. Document and gate the unsafe accessor instead.
- Rewriting the `WrappedFailure` / `callback_error_ext` machinery.
  It is intricate but well-tested. Audit the SAFETY documentation
  here, do not redesign.
- Removing the override `push_into_stack` / `from_stack` impls in
  Stage Six. Sealing the trait makes them crate-private; it does not
  delete them. They exist for performance and remain valuable.
