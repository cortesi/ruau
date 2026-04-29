#[path = "find_vendored.rs"]
mod find;

fn main() {
    println!("cargo:rerun-if-changed=build");
    println!("cargo:rerun-if-changed=shim/analyze_shim.cpp");
    find::probe_lua();
}
