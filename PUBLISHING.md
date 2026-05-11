# Publishing Checklist

Ruau is still consumed locally by nearby projects, so publishing is not just a
`cargo publish` command. Check the downstream dependency story before cutting a
release.

1. Confirm the workspace version in `Cargo.toml` is the version intended for the
   release.
2. Run the normal local preflight:
   `cargo xtask ci`.
3. Run package checks:
   `cargo publish --dry-run -p ruau`.
4. Verify removed or renamed feature flags are documented in the release notes.
5. Check downstream path dependencies:
   Verber's `verber-config`, `verber-runtime`, and `verber-mcp-client` crates
   currently point at a local Ruau checkout, and Subagent/Porter's dependency is
   versioned as `ruau = { version = "0.0.1", path = ... }`.
6. Decide whether downstream projects stay on path dependencies, move to the
   published version, or need coordinated migration commits.

## Validation

`cargo xtask ci` is the authoritative local preflight and should match CI. It
checks formatting with `rustfmt-nightly.toml`, runs clippy without applying
fixes, runs the test suite, builds docs with rustdoc warnings denied, and
enforces the unsafe-audit baseline.

Use `cargo xtask tidy` for local formatting and clippy fixes. If an intentional
unsafe change raises the audit counts, update the baseline with
`cargo xtask unsafe-audit --update-baseline` and include a reviewer-facing note
explaining why the increase is acceptable.
