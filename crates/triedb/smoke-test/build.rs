use std::env;
use std::path::PathBuf;

fn main() {
    // Get the directory where this build script is located
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let lib_dir = PathBuf::from(manifest_dir);

    // Tell cargo to look for shared libraries in the specified directory
    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    // Tell cargo to tell rustc to link our C library
    println!("cargo:rustc-link-lib=dylib=bsc_trie");

    // Only rerun this build script if the C library changes
    println!("cargo:rerun-if-changed=libbsc_trie.dylib");
}
