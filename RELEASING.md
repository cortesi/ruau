# Releasing

Publish the internal crates before the facade crate so crates.io can resolve
path-backed workspace dependencies by version:

1. `cargo publish -p ruau-luau-src`
2. `cargo publish -p ruau-sys`
3. `cargo publish -p ruau_derive`
4. `cargo publish -p ruau`

`cargo publish --dry-run -p ruau-sys` requires `ruau-luau-src` to already
exist in the crates.io index. Likewise, `cargo publish --dry-run -p ruau`
requires `ruau-sys` and `ruau_derive` to already exist in the index.
