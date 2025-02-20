//! PSA Attestation library build script
//!
//! ## Authors
//!
//! The Veracruz Development Team.
//!
//! ## Licensing and copyright notice
//!
//! See the `LICENSE_MIT.markdown` file in the Veracruz root directory for
//! information on licensing and copyright.

extern crate bindgen;

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let cc = {
        cfg_if::cfg_if! {
            if #[cfg(feature = "linux")] {
                "gcc".to_string()
            } else if #[cfg(feature = "nitro")] {
                "gcc".to_string()
            } else {
                env::var(format!("CC_{}", env::var("TARGET").unwrap().replace("-", "_"))).unwrap()
            }
        }
    };

    let project_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let target_dir = env::var("OUT_DIR").unwrap();

    let outdir_arg = format!("OUT_DIR={:}", target_dir);

    // Build the psa_attestation library, including QCBOR and t_cose
    let c_src_dir = format!("{:}/c_src/", project_dir);
    let make_status = Command::new("make")
        .env("CC", &cc)
        .current_dir(c_src_dir.clone())
        .args(&["all", outdir_arg.as_str()])
        .status()
        .unwrap();
    if !make_status.success() {
        panic!("psa_attestation C library failed to build");
    }

    println!("cargo:rustc-link-lib=static=psa_attestation");
    println!("cargo:rustc-link-search={:}", target_dir);
    // These two C libraries come from psa-crypto / psa-crypto-sys:
    println!("cargo:rustc-link-lib=static=mbedcrypto");
    println!("cargo:rustc-link-lib=static=shim");

    // Tell cargo to invalidate the build crate whenever the wrapper changes
    //println!("cargo:rerun-if-changed=wrapper.h");

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    let bindings = bindgen::Builder::default()
        // The input header we would like to generate
        // bindings for.
        .header("wrapper.h")
        // need to set ctypes_prefix to libc instead of std
        // https://github.com/rust-lang/rust-bindgen/issues/628
        .ctypes_prefix("libc")
        .clang_arg("-Ilib/t_cose/inc/")
        .clang_arg("-Ilib/QCBOR/inc/")
        // Tell cargo to invalidate the built crate whenever any of the
        // included header files changed.
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        // Finish the builder and generate the bindings.
        .generate()
        // Unwrap the Result and panic on failure.
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
