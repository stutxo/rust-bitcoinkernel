use bindgen::RustEdition;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    let (cross_cc, cross_cxx): (Option<&str>, Option<&str>) = match target.as_str() {
        t if t.starts_with("x86_64-unknown-linux-musl") => {
            (Some("x86_64-linux-musl-gcc"), Some("x86_64-linux-musl-g++"))
        }
        t if t.starts_with("aarch64-unknown-linux-musl") => (
            Some("aarch64-linux-musl-gcc"),
            Some("aarch64-linux-musl-g++"),
        ),
        _ => (None, None),
    };

    let bitcoin_dir = Path::new("bitcoin");
    let out_dir = env::var("OUT_DIR").unwrap();
    let build_dir = Path::new(&out_dir).join("bitcoin");
    let install_dir = Path::new(&out_dir).join("install");

    println!("{} {}", bitcoin_dir.display(), build_dir.display());
    // Rebuild if the submodule changes
    println!("cargo:rerun-if-changed={}", bitcoin_dir.display());

    let build_config = "RelWithDebInfo";

    // ---- Configure ----
    let mut cfg = Command::new("cmake");
    cfg.arg("-B")
        .arg(&build_dir)
        .arg("-S")
        .arg(bitcoin_dir)
        .arg(format!("-DCMAKE_BUILD_TYPE={}", build_config))
        .arg("-DBUILD_KERNEL_LIB=ON")
        .arg("-DBUILD_TESTS=OFF")
        .arg("-DBUILD_TX=OFF")
        .arg("-DBUILD_WALLET_TOOL=OFF")
        .arg("-DENABLE_WALLET=OFF")
        .arg("-DENABLE_EXTERNAL_SIGNER=OFF")
        .arg("-DBUILD_UTIL=OFF")
        .arg("-DBUILD_BITCOIN_BIN=OFF")
        .arg("-DBUILD_DAEMON=OFF")
        .arg("-DBUILD_UTIL_CHAINSTATE=OFF")
        .arg("-DBUILD_CLI=OFF")
        .arg("-DBUILD_SHARED_LIBS=OFF")
        .arg("-DCMAKE_INSTALL_LIBDIR=lib")
        .arg(format!("-DCMAKE_INSTALL_PREFIX={}", install_dir.display()));

    // Cross-compile settings for musl targets on macOS (or anywhere)
    if let (Some(cc), Some(cxx)) = (cross_cc, cross_cxx) {
        cfg.arg("-DCMAKE_SYSTEM_NAME=Linux")
            .arg(format!("-DCMAKE_C_COMPILER={}", cc))
            .arg(format!("-DCMAKE_CXX_COMPILER={}", cxx))
            // Don't try to execute test programs when cross-compiling
            .arg("-DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY")
            // Clear macOS defaults that inject -arch/-isysroot into the toolchain
            .arg("-DCMAKE_OSX_ARCHITECTURES=")
            .arg("-DCMAKE_OSX_DEPLOYMENT_TARGET=")
            .arg("-DCMAKE_OSX_SYSROOT=");
    }

    let status = cfg.status().expect("failed to run cmake (configure)");
    if !status.success() {
        panic!("cmake configure failed");
    }

    // ---- Build ----
    let num_jobs = env::var("NUM_JOBS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1);

    let status = Command::new("cmake")
        .arg("--build")
        .arg(&build_dir)
        .arg("--config")
        .arg(build_config)
        .arg(format!("--parallel={}", num_jobs))
        .status()
        .expect("failed to run cmake (--build)");
    if !status.success() {
        panic!("cmake build failed");
    }

    // ---- Install ----
    let status = Command::new("cmake")
        .arg("--install")
        .arg(&build_dir)
        .arg("--config")
        .arg(build_config)
        .status()
        .expect("failed to run cmake (--install)");
    if !status.success() {
        panic!("cmake install failed");
    }

    // ---- Link search paths ----
    // Support both single-config and multi-config generators
    let lib_dir = if install_dir.join("lib").join(build_config).exists() {
        install_dir.join("lib").join(build_config)
    } else {
        install_dir.join("lib")
    };
    if !lib_dir.exists() {
        panic!("install lib dir not found: {}", lib_dir.display());
    }
    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    // Link all static libraries found in the install directory
    for entry in fs::read_dir(&lib_dir).expect("Library directory has to be readable") {
        let path = entry.unwrap().path();
        if let Some(ext) = path.extension() {
            if ext == "a" || ext == "lib" {
                if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                    // Special case for libsecp256k1 on MSVC
                    let lib_name = if name == "libsecp256k1" && cfg!(target_env = "msvc") {
                        "libsecp256k1"
                    } else {
                        name.strip_prefix("lib").unwrap_or(name)
                    };
                    println!("cargo:rustc-link-lib=static={}", lib_name);
                }
            }
        }
    }

    // ---- Bindgen ----
    let include_path = install_dir.join("include");
    let header = include_path.join("bitcoinkernel.h");

    // Build a bindgen builder
    let mut b = bindgen::Builder::default()
        .header(header.to_str().unwrap())
        .clang_arg("-DBITCOINKERNEL_STATIC")
        .clang_arg(format!("-I{}", include_path.display()))
        .rust_target(bindgen::RustTarget::Stable_1_71)
        .rust_edition(RustEdition::Edition2021);

    // If we're cross-compiling to musl, point clang at the musl sysroot + set target
    if target.ends_with("unknown-linux-musl") {
        // Prefer the corresponding cross-CC to discover sysroot
        if let Some(cc) = cross_cc {
            if let Ok(out) = Command::new(cc).arg("-print-sysroot").output() {
                if let Ok(sysroot) = String::from_utf8(out.stdout) {
                    let sysroot = sysroot.trim();
                    if !sysroot.is_empty() {
                        // Tell clang to parse for the real target and use musl headers
                        b = b
                            .clang_arg(format!("--target={}", target))
                            .clang_arg(format!("--sysroot={}", sysroot))
                            // Some toolchains use .../include, others .../usr/include — add both
                            .clang_arg(format!("-isystem{}/include", sysroot))
                            .clang_arg(format!("-isystem{}/usr/include", sysroot));
                    }
                }
            }
        }
    }

    #[allow(deprecated)]
    let bindings = b.generate().expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set by cargo"));
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");

    // ---- Link C++ standard library ----
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    if target_os == "macos" {
        // Building for macOS target
        println!("cargo:rustc-link-lib=dylib=c++");
    } else if target_env == "musl" {
        // For musl targets, prefer static libstdc++ and expose toolchain lib paths
        if let Some(cxx) = cross_cxx {
            if let Ok(out) = Command::new(cxx).arg("-print-search-dirs").output() {
                if let Ok(s) = String::from_utf8(out.stdout) {
                    for line in s.lines() {
                        if let Some(rest) = line.strip_prefix("libraries: =") {
                            for p in rest.split(':') {
                                let p = p.trim();
                                if !p.is_empty() {
                                    println!("cargo:rustc-link-search=native={}", p);
                                }
                            }
                        }
                    }
                }
            }
        }
        println!("cargo:rustc-link-lib=static=stdc++");

        // Try to link libgcc statically only if it exists in the toolchain
        if let Some(cxx) = cross_cxx {
            if let Ok(out) = Command::new(cxx).arg("-print-file-name=libgcc.a").output() {
                if let Ok(path) = String::from_utf8(out.stdout) {
                    let path = path.trim();
                    if path != "libgcc.a" && !path.is_empty() {
                        if let Some(dir) = Path::new(path).parent() {
                            println!("cargo:rustc-link-search=native={}", dir.display());
                        }
                        println!("cargo:rustc-link-lib=static=gcc");
                    }
                }
            }
        }
    } else {
        // Generic Linux/gnu: link dynamically
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
}
