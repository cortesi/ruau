# WebAssembly Support Plan

This plan scopes WebAssembly support as a deliberate platform port, not as a promise that every
native `ruau` API will work unchanged. The first target is a reduced, useful WASM profile for
in-memory Luau execution and conversion, with native-only APIs gated behind explicit features.

## Design Rules

1. Keep native behavior unchanged unless a stage explicitly changes a feature boundary.
2. Treat `wasm32` as a constrained platform profile with a documented API subset.
3. Prefer compile-time gating over runtime stubs for APIs that cannot work on WASM.
4. Keep the first WASM milestone small: runtime, compiler, values, userdata, and in-memory require.
5. Defer worker threads, filesystem paths, heap dumps, and blocking analysis until they have a
   concrete WASM host story.

1. Stage One: Define The Supported WASM Profile

Write down the target matrix and supported API surface before touching build logic. This stage
prevents the port from drifting into unclear partial support.

1. [ ] Add a "WebAssembly support" section to `README.md` that describes the first supported
   profile: `wasm32-unknown-unknown` for Rust/WASM consumers, and an optional Emscripten/browser
   route for Luau's C++ web build.
2. [ ] State that native-only APIs are out of the first milestone: `LuauWorker`,
   `FilesystemResolver`, path-based checker APIs, path-based chunk loading, heap dumps, and any
   API requiring OS threads, blocking pools, host files, sockets, or libc `FILE*`.
3. [ ] State that in-memory APIs are in scope: `Luau`, `Compiler`, chunk loading from strings,
   value conversion, userdata, serde conversion, `InMemoryResolver`, and runtime `require` through
   an in-memory resolver.
4. [ ] Add a short internal note to `crates/ruau/src/lib.rs` docs explaining that direct WASM mode
   uses caller-provided future execution and does not include native worker integration.
5. [ ] Decide whether type analysis belongs in the first milestone. If it remains in scope, define
   whether it runs synchronously on WASM or uses host-specific Web Worker integration later.

2. Stage Two: Split Cargo Features Along Platform Boundaries

Move native-only dependencies and modules behind feature gates so `cargo check` can reach the C++
build on WASM without pulling in unsupported Rust dependencies.

1. [ ] Replace the unconditional Tokio dependency in `crates/ruau/Cargo.toml` with a minimal
   feature set that excludes `net`, `fs`, and blocking-pool assumptions on `target_family = "wasm"`.
2. [ ] Introduce explicit crate features for native-only surfaces, such as `native-io`,
   `filesystem`, `analysis`, `worker`, and `heap-dump`. Pick final names before landing.
3. [ ] Gate `crates/ruau/src/worker.rs` and its public re-exports behind a native-only feature and
   `not(target_family = "wasm")`.
4. [ ] Gate `FilesystemResolver`, path-based resolver tests, and any path-based chunk loading
   behind `filesystem` plus `not(target_family = "wasm")`.
5. [ ] Gate `HeapDump` and `Luau::heap_dump` behind `heap-dump` plus
   `not(target_family = "wasm")` because the current implementation uses libc temp files.
6. [ ] Gate checker methods that rely on `tokio::task::spawn_blocking` or host file reads. Leave a
   follow-up path for a synchronous or host-worker-backed WASM analyzer if Stage One keeps analysis
   in scope.
7. [ ] Add compile-fail or `cargo check` coverage for `--target wasm32-unknown-unknown` with the
   intended minimal feature set.

3. Stage Three: Make The Vendored Luau Build WASM-Aware

Teach `ruau-luau-src` and `ruau-sys` to build a WASM-compatible native library set instead of
trying the normal host C++ build unchanged.

1. [ ] Add a build-mode option to `crates/ruau-luau-src/src/lib.rs` that detects `wasm32` targets
   and chooses a WASM source set.
2. [ ] Stop compiling Luau CodeGen for WASM targets. The current build panics for Emscripten and
   CodeGen is not needed for the first interpreted-runtime milestone.
3. [ ] Split Luau libraries into runtime, compiler, analysis, require, and codegen groups so WASM
   targets can omit unsupported or out-of-scope groups.
4. [ ] Add build-script support for a C++ WASM sysroot. For `wasm32-unknown-unknown`, document the
   required Clang/libc++ setup or reject the target early with a clear build error.
5. [ ] Add an Emscripten path that uses `em++` flags when targeting `wasm32-unknown-emscripten`,
   including exception flags compatible with Luau's web build.
6. [ ] Consider reusing upstream Luau's `LUAU_BUILD_WEB` CMake route for browser smoke tests rather
   than duplicating all Emscripten link options in the Rust build helper.
7. [ ] Keep `ruau-sys`'s C shim build aligned with the selected library groups. If analysis is
   disabled for the first WASM profile, do not compile `shim/analyze_shim.cpp`.

4. Stage Four: Port The High-Level Runtime Surface

Once the native library links, make the safe Rust layer compile and work for in-memory runtime use.

1. [ ] Run `cargo check -p ruau --target wasm32-unknown-unknown --no-default-features` and fix
   remaining target-specific Rust errors without weakening native API guarantees.
2. [ ] Audit FFI callback ABIs on WASM, especially `extern "C-unwind"` and panic handling. Decide
   whether WASM builds must force `LUA_USE_LONGJMP` or disable cross-FFI unwinding.
3. [ ] Verify `Value`, `Table`, `Function`, `Thread`, userdata, strings, buffers, and vectors on
   32-bit WASM pointer width. Add focused tests for layout assumptions that currently branch on
   `target_pointer_width`.
4. [ ] Ensure `StdLib` choices are documented for WASM. Confirm `os`, `debug`, and any host-facing
   library behavior is either available, sandboxed, or excluded from the recommended profile.
5. [ ] Keep runtime `require` support limited to resolver-backed in-memory modules for the first
   milestone. Do not reintroduce filesystem resolution through browser or WASI shims yet.
6. [ ] Add a small `wasm-smoke` crate or example that creates a `Luau`, evaluates a string chunk,
   converts values both ways, and calls a Rust host function from Luau.

5. Stage Five: Decide The Analyzer Story

The analyzer is valuable in browser tooling, but it has different constraints from runtime
execution. Land it only after the runtime profile is stable.

1. [ ] If analysis is deferred, gate `ruau::analyzer` and `HostApi::add_definitions_to` on an
   `analysis` feature that is disabled for the minimal WASM profile.
2. [ ] If analysis is included, remove `spawn_blocking` from the WASM path. Either run analysis
   synchronously for small inputs or expose a host-provided worker strategy in a later design.
3. [ ] Split path-based analyzer APIs from string and in-memory module APIs so browser consumers do
   not need fake filesystem support.
4. [ ] Add WASM-compatible tests for `Checker::check`, `ModuleInterfaceSet`, and
   `check_with_interfaces` if the analyzer is included.
5. [ ] Document cancellation semantics for WASM analysis. Native cancellation tokens may remain,
   but dropped futures cannot rely on Tokio blocking-task cancellation.

6. Stage Six: Add WASM CI And Tooling

Make WASM support visible in CI before calling it supported.

1. [ ] Add a CI job that installs the chosen WASM C++ toolchain and runs the minimal
   `cargo check -p ruau --target wasm32-unknown-unknown --no-default-features` command.
2. [ ] Add a browser or Node smoke test using `wasm-bindgen-test`, `wasmtime`, or an Emscripten
   runner. Pick one host and document why it matches the supported target.
3. [ ] Add a separate Emscripten smoke job if the project chooses to support
   `wasm32-unknown-emscripten`.
4. [ ] Ensure native CI still runs the full default feature set, worker tests, filesystem tests,
   heap dump tests, and analyzer tests.
5. [ ] Add a troubleshooting section for common WASM build failures: missing libc++ headers,
   missing `em++`, unsupported CodeGen, and Tokio features that accidentally pull in `mio`.

7. Stage Seven: Stabilize The Public Contract

After the smoke path works, polish the API boundary so users can depend on the platform story.

1. [ ] Add rustdoc `doc(cfg(...))` annotations for native-only modules and WASM-compatible modules.
2. [ ] Add examples showing the supported WASM path and native-only alternatives.
3. [ ] Add release notes documenting the exact supported targets, required toolchains, and omitted
   APIs.
4. [ ] Decide whether to add a convenience feature such as `wasm-runtime` that selects the intended
   minimal set for browser consumers.
5. [ ] Re-run public API review for `wasm32` docs to ensure native-only items are not presented as
   available in the WASM profile.
