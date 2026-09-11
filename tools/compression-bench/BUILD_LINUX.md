# Static Linux benchmark build

`build_linux.py` builds only the isolated compression experiment, not production binaries. It mounts the checkout/source archive directory read-only and writes architecture-separated artifacts, Cargo caches, target directories, source snapshots, logs and manifests under `/private/tmp/flowsplice-compression-linux/{amd64,arm64}`.

## Reproduce

```sh
python3 tools/compression-bench/build_linux.py --arch amd64 --stage native
python3 tools/compression-bench/build_linux.py --arch arm64 --stage native
# Only after the benchmark source is finalized and the root-exported
# frozen-corpus.bin is present in /private/tmp/flowsplice-compression-linux:
python3 tools/compression-bench/build_linux.py --arch amd64 --stage final
python3 tools/compression-bench/build_linux.py --arch arm64 --stage final
```

All container invocations use `docker run --pull=never --platform linux/<arch>` and the already cached immutable base `rust:1.97-alpine@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900`. Both platform variants are locally available. No image pull/refresh occurs; a missing variant fails. The script enforces exact Rust 1.97.1 and the correct native musl target. Package installation is limited to disposable local build containers; nothing is installed on a VPS/router or the macOS host. Package versions are captured in `build.json`; the Alpine package repository itself is not snapshot-pinned.

## Codecs, CPU baseline and linkage

Official Snappy 1.2.2, Zstandard 1.5.7 and LZ4 1.10.0 tarballs are copied from the existing verified archive directory, SHA-256 checked against `prepare_native.py`, and extracted per architecture. No upstream source patch is applied. Snappy exposes both official compression levels. C/C++ compilation uses `-O3 -DNDEBUG -ffunction-sections -fdata-sections` plus:

- x86_64: `-march=x86-64-v3`, the common AVX2/BMI2/SSE4-class baseline requested for the three inspected VPS CPUs. These binaries are not intended for older x86 hosts.
- AArch64: `-march=armv8-a+crc`, generic ARMv8-A/NEON plus CRC matching the router, with no SVE or host-native tuning.

Architecture flags are applied to CMake **feature probes as well as actual compilation**. Release-only flags originally caused incorrect Snappy feature detection; the corrected script clears its CMake build directories before the native stage. Generated Snappy `config.h`, compiler versions, exact commands and source/archive hashes are recorded in each architecture's `build.json`.

Rust retains the benchmark's release O3/thin-LTO configuration and adds `+crt-static`, `-static`, `-static-libstdc++`, and `-static-libgcc`. Native codec archives and the C++ shim are static; the final ELF check rejects any interpreter or `DT_NEEDED` entry. This includes the C++ runtime, not just musl libc.

## Validation and interpretation

The final stage runs all four benchmark release tests in the matching local container, then builds and inspects the final ELF with `file` and `readelf`. Tests exercise pinned codec versions, boundaries, frozen-corpus behavior and the deterministic corpus. The final stage then runs `--bundle /out/frozen-corpus.bin OUT 1 5` using the identical root-exported bundle and requires 200 cases and peak RSS below 256 MiB. This is explicitly labeled a correctness-only smoke; its timing values are not benchmark results. The local x86 container may be emulated on Apple Silicon; these runs establish **correctness only**, never remote throughput. Real timing is performed separately on the actual VPS/router. Nothing here changes production services, packages or configuration.

Evidence and output paths for each architecture:

- `build.json`: toolchain/package versions, CPU flags, generated Snappy configuration, commands, source/archive/binary/bundle SHA-256, ELF checks and correctness-smoke peak RSS.
- `build.log`: complete command stdout/stderr, including initial failures if retried.
- `flowsplice-compression-bench`: final checked static executable, once the final stage passes.

Docker's documented [`--pull=never` behavior](https://docs.docker.com/reference/cli/docker/container/run/#pull) prevents fallback downloads when a cached image is absent.

## Verified artifacts (2026-09-11)

Both architectures passed all four Release tests and all 200 frozen-bundle smoke cases. GCC/G++ is Alpine 15.2.0. Snappy configuration enables SSSE3, x86 CRC32 and BMI2 for amd64; NEON and ARM CRC32 for arm64.

| Architecture | Bytes | Smoke peak RSS | SHA-256 |
| --- | ---: | ---: | --- |
| amd64 | 2,110,456 | 103,904 KiB | `9e48556fc64ef35c56765e4b95ea50881e5db8f37e27561764e54c557bddad8d` |
| arm64 | 1,853,712 | 101,504 KiB | `45a51cff479cc3d9b8353264aced82318988e9b9fa72209d76e07d7594bd4c34` |

The amd64 executable is static PIE (a relocation dynamic section exists, but no `DT_NEEDED` and no interpreter); arm64 is static ELF without a dynamic section. Both include their C++ runtime. Successful smoke results reside at `<architecture>/smoke/results.json`. The initial builder expected the output argument to be a JSON file rather than a directory; this post-smoke metadata-read error was corrected, and the already successful smoke outputs were reconciled into the manifests without rerunning. The manifests explicitly record this evidence reconciliation; `build.log` retains the original commands.
