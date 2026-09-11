# Mobile decoder cross-compilation feasibility

Checked 2026-09-11 on the existing Mac toolchains, without installing tools or modifying system configuration. **All three targets compiled and linked successfully. No mobile executable was run, and no mobile compression benchmark was performed.**

| Target | Installed SDK/toolchain | Deployment floor | Result |
| --- | --- | --- | --- |
| iOS device arm64 / `aarch64-apple-ios` | Xcode 26.6 (17F113), iPhoneOS SDK 26.5, Apple Clang 21.0.0 (`clang-2100.1.1.101`) | iOS 17.0 | Static libraries and Rust-to-decoder link passed; Mach-O arm64, platform 2 |
| iOS simulator arm64 / `aarch64-apple-ios-sim` | Same Xcode/Clang, iPhoneSimulator SDK 26.5 | iOS 17.0 | Static libraries and Rust-to-decoder link passed; Mach-O arm64, platform 7 |
| Android arm64-v8a / `aarch64-linux-android` | NDK r29, 29.0.14206865, Android Clang 21.0.0 (13989888), existing `darwin-x86_64` toolchain | API 34 | Static libraries and Rust-to-decoder link passed; ELF64 AArch64 PIE, `/system/bin/linker64` |

The deployment floors match the repository's current Apple project and Android native-build script, not a newly chosen minimum. The globally selected developer directory is CommandLineTools; the check uses a **process-local** `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`, matching the repository's Apple build convention. All three Rust targets were already installed.

## What was proved

1. The already downloaded, SHA-verified official Snappy 1.2.2, Zstandard 1.5.7 and LZ4 1.10.0 sources compile to each target's static archive. Zstd multithreading is disabled as in the native benchmark. LZ4 builds `lz4.c` and `lz4hc.c`; upstream tests/benchmarks/programs are disabled. Archive architecture is recorded using `lipo` on Apple and LLVM archive-member headers on Android.
2. The unchanged benchmark `native.cc` also compiles into an archive on each target. This is a **compile feasibility check only**, not a requirement that mobile clients use its encoder/context interface.
3. Following the decoder-only mobile requirement, the **final** link probe does not link the benchmark shim. A separate temporary decoder-only C ABI shim calls only `snappy::GetUncompressedLength`, `snappy::RawUncompress`, `ZSTD_decompress`, `ZSTD_isError`, and `LZ4_decompress_safe`, plus returns compile-time version text. A separate `xcrun nm -u` inspection of the iOS device decoder object confirms exactly these five unresolved codec symbols and no compression/context-creation references.
4. A small `no_std` Rust static library for each installed target calls that decoder shim; a C++ executable links the Rust object, decoder shim, official static codec archives and platform runtime successfully. Apple links with `-dead_strip`; Android uses `--gc-sections` and static libc++. The executable is a **link-only artifact**; its dummy input is not a valid runtime test fixture.

## Reproduce

After `prepare_native.py` has populated the verified sources:

```sh
python3 tools/compression-bench/check_cross_compile.py \
  --sources /private/tmp/flowsplice-compression-bench/native \
  --output /private/tmp/flowsplice-compression-cross
```

The script installs nothing, downloads nothing, and only builds beneath the supplied output directory. It clears its own per-target CMake build subdirectories before a full build to avoid stale SDK/CPU feature probes. `--link-only` reuses completed archives and repeats only decoder/Rust link checks. Default NDK and Xcode paths are overrideable with `--ndk` and `--developer-dir`.

Evidence is in `/private/tmp/flowsplice-compression-cross/build.log` (all exact commands, stdout/stderr, including initial failed attempts and final successful runs) and `cross-build.json` (last invocation's target metadata, commands, archive hashes/architectures and executable load commands). The final manifest at that exact path records decoder-only compile and link commands for all three targets: each executable link includes `decoder.o` and excludes `libbench_native.a`. The `decoder_undefined_symbols` manifest field was added to the script after this successful run; it is absent from this saved manifest, and no subsequent rebuild was performed. The separate iOS-device symbol inspection was read-only. Each target directory contains archives, `decoder.o`, `librust_probe.a`, and `link-probe`. Initial harness issues with mixed stdout/stderr SDK-path parsing and stale CMake compiler probes were corrected; the clean rebuild and subsequent decoder-only links passed for all targets.

## Limits and remaining integration work

This establishes native source and Rust/C++ link feasibility, not production integration or target runtime correctness. It does not validate a full Rust `std` application, Swift/JNI integration, device/simulator execution, packaging, signing, app-store delivery, thread scheduling, mobile performance, or malformed-input behavior on mobile. Those checks require the requested integration/runtime test scope later. No product configuration, Rust workspace dependencies, app bundles or release artifacts were modified.

Full upstream archives include encoder code, but **mobile clients only need decompression**. Archive size is not final delivered code size. Dead stripping is enabled for the probe, but archive granularity and compiled section boundaries can retain unused code; this check does not claim a decoder-only production size or optimal pruning. Android links static libc++ in this standalone proof; a production app must coordinate its C++ runtime choice with its other native components.

Official configuration references: [Android NDK CMake toolchain, ABI and API settings](https://developer.android.com/ndk/guides/cmake), [Android ABI definitions](https://developer.android.com/ndk/guides/abis). Actual versions and architectures above were measured from the installed tools and artifacts, not inferred from those references.
