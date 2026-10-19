use std::env;

fn main() {
    // Cargo sets CARGO_CFG_TARGET_ARCH to the target arch (x86_64, aarch64, ...).
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "unknown".to_string());
    // Encode the target arch in the SONAME so a wheel built for one arch is not loaded on another.
    // The Makefile also passes it via RUSTFLAGS (cargo does not forward build-script link args).
    println!("cargo:rustc-link-args=-Wl,-soname,libdatatraffic.so.1.{arch}");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_ARCH");
}
