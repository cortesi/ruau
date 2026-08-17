# Changelog

## Unreleased

## 0.4.0 - 2026-08-18

### Changed

- Breaking: `ruau-syntax`, `ruau-declaration`, `ruau-derive`,
  `ruau-filesystem`, `ruau-session`, and `ruau-executor` replace the 0.3 crate
  names. Analysis is consolidated into `ruau-typecheck`, and the low-level VM
  API is consolidated into `ruau-vm`.
- The supported upstream baseline is Luau 0.734 and bytecode version 9.
- Host integers become Luau numbers when they are exactly representable in
  IEEE-754 binary64 (`|v| <= 2^53`). Larger values stay as the distinct VM
  integer type. The rule applies to `IntoLua`, the serde serializer, both JSON
  decoders, and `Evaluator` arguments.
- `MarshaledJsonOptions::default()`, `marshaled_to_json`, and `Evaluator`
  output write exactly-integral finite numbers that fit in `i64` as JSON
  integers, including canonicalizing `-0.0` to zero.
  `MarshaledJsonOptions::strict()` keeps exact float-ness.
- `ModuleArray` constants carry the protected JSON-array marker, so evaluator
  argument arrays preserve empty-array identity. Evaluator argument `null`
  values now use the same sentinel as other JSON document decoders.

### Added

- Checked source graphs, typed native modules and declarations, retained and
  detached sessions, validated filesystem mount sets, and bounded multi-tenant
  execution.
- Deterministic JSON Schema declaration lowering and a ready-made native JSON
  module.
- `JsonDecodeOptions` / `JsonNullPolicy` with `document()` and `typed()`
  presets, plus `json_to_scoped_value_with_options`,
  `json_to_marshaled_with_options`, and `json_host_return_with_options`.
- `json.array` and `is_json_array_table` for the protected JSON array marker.
  `json.array` now refuses to replace an existing non-JSON metatable.

### Fixed

- VM entrypoints reclaim garbage at call boundaries, preserve cancellation and
  snapshot invariants, and use no executable unsafe Rust.
