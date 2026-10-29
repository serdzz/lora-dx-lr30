use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");

    // Rebuild when DEVICE_ID changes so `option_env!("DEVICE_ID")` in
    // device_id.rs actually picks up a new value instead of reusing a
    // cached build. Cargo doesn't track env vars used only by `option_env!`
    // on its own.
    println!("cargo:rerun-if-env-changed=DEVICE_ID");
}
