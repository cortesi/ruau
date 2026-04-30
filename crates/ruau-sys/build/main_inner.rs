/// Vendored Luau build probing.
mod find_vendored;

/// Build script entrypoint.
fn main() {
    println!("cargo:rerun-if-changed=build");
    println!("cargo:rerun-if-changed=shim/analyze_shim.cpp");
    find_vendored::probe_lua();
}
