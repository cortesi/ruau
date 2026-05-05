/// Builds vendored Luau and emits link metadata for ruau-sys.
pub fn probe_lua() {
    let artifacts = ruau_luau_src::Build::new().set_max_cstack_size(1_000_000).build();

    let mut shim = cc::Build::new();
    shim.warnings(false)
        .cargo_metadata(false)
        .std("c++17")
        .cpp(true)
        .file("shim/analyze_shim.cpp")
        .out_dir(artifacts.lib_dir());
    ruau_luau_src::configure_cc_archiver(&mut shim);

    for include_path in artifacts.include_paths() {
        shim.include(include_path);
    }

    shim.compile("ruauanalyze");

    println!("cargo:rustc-link-search=native={}", artifacts.lib_dir().display());
    println!("cargo:rustc-link-lib=static=ruauanalyze");
    for lib in artifacts.libs() {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    if let Some(cpp_stdlib) = artifacts.cpp_stdlib() {
        println!("cargo:rustc-link-lib={cpp_stdlib}");
    }

    println!("cargo:rustc-env=LUAU_VERSION={}", artifacts.version());
    println!(
        "cargo:rustc-env=RUAU_LUAU_SOURCE_ROOT={}",
        artifacts.source_root().display()
    );
    for include_path in artifacts.include_paths() {
        println!(
            "cargo:rustc-env=RUAU_LUAU_INCLUDE_PATH={}",
            include_path.display()
        );
    }
}
