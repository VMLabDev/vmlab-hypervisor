// Link the oracle static library produced by `just tcg-oracle-build`.
// Point VMLAB_HV_ORACLE_DIR at the directory containing liboracle.a to
// override the default target/tcg/oracle location.

use std::{env, path::PathBuf};

fn main() {
    let dir = match env::var("VMLAB_HV_ORACLE_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => {
            let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
            let dir = manifest.join("../../../target/tcg/oracle");
            if !dir.join("liboracle.a").exists() {
                panic!(
                    "no oracle at {} — run `just tcg-oracle-build` first",
                    dir.display()
                );
            }
            dir
        }
    };
    println!("cargo:rerun-if-changed={}", dir.join("liboracle.a").display());
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=oracle");
    // The Sail C runtime uses GMP for arbitrary-precision integers, the
    // model objects are C++, and elf loading pulls in zlib.
    println!("cargo:rustc-link-lib=dylib=gmp");
    println!("cargo:rustc-link-lib=dylib=z");
    println!("cargo:rustc-link-lib=dylib=stdc++");
}
