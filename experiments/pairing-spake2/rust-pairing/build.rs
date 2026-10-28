use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CROSSFLOW_BORINGSSL_LIB_DIR");
    println!("cargo:rerun-if-changed=../source-lock.json");
    let dir = PathBuf::from(env::var_os("CROSSFLOW_BORINGSSL_LIB_DIR").expect(
        "Set CROSSFLOW_BORINGSSL_LIB_DIR to the crypto static-library directory built from ../source-lock.json; this experiment never downloads native dependencies",
    ));
    assert!(
        dir.is_absolute(),
        "CROSSFLOW_BORINGSSL_LIB_DIR must be absolute"
    );
    let archive = dir.join("libcrypto.a");
    assert!(archive.is_file(), "Expected pinned BoringSSL libcrypto.a");
    println!("cargo:rerun-if-changed={}", archive.display());
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=crypto");
    match env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "windows" => {
            assert_eq!(
                env::var("CARGO_CFG_TARGET_ENV").unwrap(),
                "gnu",
                "Only the tested LLVM-MinGW native build is supported on Windows"
            );
            println!("cargo:rustc-link-lib=dylib=c++");
            println!("cargo:rustc-link-lib=dylib=unwind");
            println!("cargo:rustc-link-lib=dylib=winpthread");
            println!("cargo:rustc-link-lib=dylib=ws2_32");
        }
        "macos" => println!("cargo:rustc-link-lib=dylib=c++"),
        _ => panic!("This native build experiment currently supports Windows GNU and macOS only"),
    }
}
