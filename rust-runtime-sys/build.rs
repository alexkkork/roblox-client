use std::{env, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let root = manifest.parent().expect("workspace root");
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("CMakeLists.txt").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("build.rs").display()
    );
    let luau = root.join("vendor/luau");
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("vendor/luau-Require").display()
    );
    println!("cargo:rerun-if-changed={}", luau.display());

    let mut config = cmake::Config::new(&manifest);
    config.define("ROVE_LUAU_SOURCE", &luau);
    // Multi-configuration generators (Visual Studio) also need --config Release.
    // Setting only CMAKE_BUILD_TYPE lets their Debug CRT override Rust's CRT.
    config.profile("Release");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let static_crt = env::var("CARGO_CFG_TARGET_FEATURE")
            .unwrap_or_default()
            .split(',')
            .any(|feature| feature == "crt-static");
        config.static_crt(static_crt);
        config.define(
            "CMAKE_MSVC_RUNTIME_LIBRARY",
            if static_crt {
                "MultiThreaded"
            } else {
                "MultiThreadedDLL"
            },
        );
        // cmake-rs otherwise replaces MSVC's Release flags with its CRT flags.
        // Keep the VM optimized even inside an unoptimized Cargo test build.
        config.define("CMAKE_CXX_FLAGS_RELEASE", "/O2 /DNDEBUG");
    }
    let dst = config.build();
    println!(
        "cargo:rustc-link-search=native={}",
        dst.join("lib").display()
    );

    for lib in [
        "Luau.Compiler",
        "Luau.Require",
        "Luau.VM",
        "Luau.Bytecode",
        "Luau.Config",
        "Luau.Ast",
        "Luau.Common",
    ] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => println!("cargo:rustc-link-lib=c++"),
        Ok("linux") => println!("cargo:rustc-link-lib=stdc++"),
        Ok("windows") => {}
        other => panic!("unsupported target OS: {other:?}"),
    }
}
