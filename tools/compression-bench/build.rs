fn main() {
    let dir = std::env::var("BENCH_NATIVE_DIR")
        .expect("run prepare_native.py and set BENCH_NATIVE_DIR to its output directory");
    println!("cargo:rerun-if-env-changed=BENCH_NATIVE_DIR");
    println!("cargo:rerun-if-changed=native.cc");
    println!("cargo:rustc-link-search=native={dir}/lib");
    for name in ["bench_native", "snappy", "zstd", "lz4"] {
        println!("cargo:rustc-link-lib=static={name}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=c++");
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
}
