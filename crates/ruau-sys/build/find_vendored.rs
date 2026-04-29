#![allow(dead_code)]

pub fn probe_lua() {
    let artifacts = ruau_luau_src::Build::new()
        .set_max_cstack_size(1_000_000)
        .build();

    artifacts.print_cargo_metadata();
}
