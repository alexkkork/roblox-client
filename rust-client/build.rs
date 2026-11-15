use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn track_runtime(path: &Path) {
    for entry in fs::read_dir(path).expect("runtime sources unavailable") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            if !matches!(
                entry.file_name().to_str(),
                Some("build" | "launcher" | "tests" | "__pycache__")
            ) {
                track_runtime(&path);
            }
        } else if path.file_name().and_then(|v| v.to_str()) == Some("CMakeLists.txt")
            || matches!(
                path.extension().and_then(|v| v.to_str()),
                Some("cpp" | "h" | "hpp" | "inc" | "json" | "cmake" | "py")
            )
        {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn checked(command: &mut Command) {
    let status = command.status().expect("could not start native build tool");
    assert!(status.success(), "native build failed: {command:?}");
}

fn main() {
    if env::var_os("CARGO_FEATURE_RUST_HOST").is_some() {
        // the rust host links only the pinned vm through rove-runtime-sys
        return;
    }
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let runtime = root.join("runtime");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("native");
    let toolchain = env::var_os("ROVE_CMAKE_TOOLCHAIN_FILE");
    assert!(
        env::var("HOST").unwrap() == env::var("TARGET").unwrap() || toolchain.is_some(),
        "cross-compilation requires ROVE_CMAKE_TOOLCHAIN_FILE and a Cargo target linker"
    );
    let mut configure = Command::new("cmake");
    configure
        .arg("-S")
        .arg(&runtime)
        .arg("-B")
        .arg(&out)
        .arg("-DCMAKE_BUILD_TYPE=Release");
    // cmake under rosetta can pick x86_64 even when cargo is building arm64.
    // make the native libraries match the rust executable or linking breaks.
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "macos" {
        let architecture = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
            "aarch64" => "arm64",
            "x86_64" => "x86_64",
            other => panic!("unsupported macOS native architecture: {other}"),
        };
        configure.arg(format!("-DCMAKE_OSX_ARCHITECTURES={architecture}"));
    }
    if let Some(path) = toolchain {
        let path = PathBuf::from(path)
            .canonicalize()
            .expect("CMake toolchain file is unavailable");
        println!("cargo:rerun-if-changed={}", path.display());
        configure.arg(format!("-DCMAKE_TOOLCHAIN_FILE={}", path.display()));
    }
    checked(&mut configure);
    let jobs = env::var("NUM_JOBS").unwrap_or("2".into());
    checked(Command::new("cmake").arg("--build").arg(&out).args([
        "--config",
        "Release",
        "--target",
        "headless_luau_bridge",
        "--parallel",
        &jobs,
    ]));
    let allocator_probe = env::var_os("CARGO_FEATURE_ALLOCATOR_MIMALLOC").is_some()
        && env::var("CARGO_CFG_TARGET_OS").unwrap() == "macos";
    if allocator_probe {
        checked(Command::new("cmake").arg("--build").arg(&out).args([
            "--config",
            "Release",
            "--target",
            "mimalloc_ownership_probe",
            "--parallel",
            &jobs,
        ]));
        println!(
            "cargo:rerun-if-changed={}",
            runtime.join("tests/mimalloc_ownership_probe.cpp").display()
        );
    }
    // watch the .inc files too, so editing a binding actually triggers a rebuild.
    track_runtime(&runtime);
    println!(
        "cargo:rerun-if-changed={}",
        root.join("vendor/luau").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        root.join("vendor/lz4").display()
    );
    for variable in [
        "CXX",
        "CC",
        "CMAKE_GENERATOR",
        "CMAKE_PREFIX_PATH",
        "ROVE_CMAKE_TOOLCHAIN_FILE",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    println!("cargo:rustc-link-search=native={}", out.display());
    if allocator_probe {
        println!("cargo:rustc-link-lib=static=mimalloc_ownership_probe");
    }
    for lib in [
        "headless_luau_bridge",
        "Luau.Compiler",
        "Luau.VM",
        "Luau.Bytecode",
        "Luau.Config",
        "Luau.Ast",
        "Luau.Common",
    ] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    match env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "macos" => println!("cargo:rustc-link-lib=c++"),
        "linux" => {
            println!("cargo:rustc-link-lib=stdc++");
            println!("cargo:rustc-link-lib=dl");
            println!("cargo:rustc-link-lib=static=rove_lz4");
        }
        "windows" => {
            println!("cargo:rustc-link-lib=bcrypt");
            println!("cargo:rustc-link-lib=static=rove_lz4");
            if let Some(prefix) = env::var_os("CMAKE_PREFIX_PATH") {
                for path in env::split_paths(&prefix) {
                    println!(
                        "cargo:rustc-link-search=native={}",
                        path.join("lib").display()
                    );
                }
            }
            if env::var("CARGO_CFG_TARGET_ENV").unwrap() == "gnu" {
                println!("cargo:rustc-link-lib=stdc++");
                // Rust's windows-gnu standard library already supplies its
                // pthread archive. A second DLL import duplicates its symbols.
            }
        }
        other => panic!("native runtime platform not yet supported: {other}"),
    }
    if env::var("CARGO_CFG_TARGET_OS").unwrap() != "windows" {
        println!("cargo:rustc-link-lib=pthread");
    }
}
