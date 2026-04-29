#![allow(dead_code)]

pub fn probe_lua() {
    let artifacts = luau0_src::Build::new()
        .enable_codegen(true)
        .set_max_cstack_size(1000000)
        .set_vector_size(3)
        .build();

    artifacts.print_cargo_metadata();
}
