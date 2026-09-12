fn main() {
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo:rerun-if-changed=bridge.cc");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")
        .unwrap_or_else(|error| panic!("Cargo target metadata missing: {error}"));
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV")
        .unwrap_or_else(|error| panic!("Cargo target metadata missing: {error}"));
    let mut config = cmake::Config::new("vendor/snappy");
    if target_os == "android" {
        // Respect the caller's NDK compiler and API level for CMake probes too.
        let compiler = cc::Build::new().get_compiler();
        let path = compiler.path();
        let ndk = path
            .ancestors()
            .nth(6)
            .unwrap_or_else(|| panic!("Android compiler must be inside an NDK toolchain"));
        let name = path
            .file_name()
            .unwrap_or_else(|| panic!("Android compiler path has no file name"))
            .to_string_lossy();
        let api = name
            .strip_suffix("-clang")
            .and_then(|name| name.rsplit_once("android").map(|(_, api)| api))
            .filter(|api| !api.is_empty() && api.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or_else(|| panic!("Android CC must be the NDK API-qualified clang wrapper"));
        config
            .define("CMAKE_ANDROID_NDK", ndk)
            .define("CMAKE_SYSTEM_VERSION", api)
            .define("CMAKE_ANDROID_STL_TYPE", "c++_static");
    }
    let dst = config
        .define("SNAPPY_BUILD_TESTS", "OFF")
        .define("SNAPPY_BUILD_BENCHMARKS", "OFF")
        .define("SNAPPY_REQUIRE_AVX", "OFF")
        .define("SNAPPY_REQUIRE_AVX2", "OFF")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .define("CMAKE_INSTALL_LIBDIR", "lib")
        .cxxflag("-ffunction-sections")
        .cxxflag("-fdata-sections")
        .build();
    let mut bridge = cc::Build::new();
    bridge
        .cpp(true)
        .std("c++11")
        .file("bridge.cc")
        .include(dst.join("include"))
        .flag_if_supported("-fno-exceptions")
        .flag_if_supported("-fno-rtti")
        .flag_if_supported("-ffunction-sections")
        .flag_if_supported("-fdata-sections");
    if std::env::var_os("CARGO_FEATURE_ENCODE").is_some() {
        bridge.define("FLOWSPLICE_SNAPPY_ENCODE", None);
    }
    if target_os == "android" {
        // The Android product contains one Rust JNI shared library.
        bridge.cpp_link_stdlib(None);
    }
    if target_env == "musl" || target_os == "android" {
        expose_static_runtime(&bridge, &dst, &target_os);
        bridge.cpp_link_stdlib(None);
    }
    bridge.compile("flowsplice_snappy_bridge");
    println!(
        "cargo:rustc-link-search=native={}",
        dst.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=snappy");
    if target_os == "android" {
        // Bundle the runtime after Snappy; a cdylib otherwise permits unresolved C++.
        println!("cargo:rustc-link-lib=static=c++_static");
        println!("cargo:rustc-link-lib=static=c++abi");
    }
    if target_env == "musl" {
        // GNU ld processes archives left to right: runtime follows Snappy.
        println!("cargo:rustc-link-lib=static=stdc++");
    }
}

fn expose_static_runtime(bridge: &cc::Build, dst: &std::path::Path, target_os: &str) {
    let archive = if target_os == "android" {
        "libc++_static.a"
    } else {
        "libstdc++.a"
    };
    // rustc bundles this archive into the rlib, so GCC's private search
    // directory must be explicit even though the compiler driver knows it.
    let output = bridge
        .get_compiler()
        .to_command()
        .arg(format!("-print-file-name={archive}"))
        .output()
        .unwrap_or_else(|error| panic!("Cannot query target C++ static runtime: {error}"));
    assert!(
        output.status.success(),
        "Target C++ static runtime query failed"
    );
    let runtime = String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("Invalid target C++ runtime path: {error}"));
    let runtime = std::path::Path::new(runtime.trim());
    assert!(
        runtime.is_absolute() && runtime.is_file(),
        "Target C++ compiler did not locate {archive}: {}",
        runtime.display()
    );
    let directory = runtime
        .parent()
        .unwrap_or_else(|| panic!("Target C++ runtime has no parent directory"));
    if target_os == "android" {
        // Do not expose the NDK common library directory: it also contains
        // libc.a and would shadow API-specific shared system libraries.
        let private = dst.join("cxx-runtime");
        std::fs::create_dir_all(&private)
            .unwrap_or_else(|error| panic!("Cannot create C++ runtime directory: {error}"));
        for archive in ["libc++_static.a", "libc++abi.a"] {
            std::fs::copy(directory.join(archive), private.join(archive))
                .unwrap_or_else(|error| panic!("Cannot copy target {archive}: {error}"));
        }
        println!("cargo:rustc-link-search=native={}", private.display());
    } else {
        println!("cargo:rustc-link-search=native={}", directory.display());
    }
}
