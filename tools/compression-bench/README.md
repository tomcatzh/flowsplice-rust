# PTY compression experiment

This standalone Rust package compares upstream Zstd 1.5.7 (`-3`, `1`, `3`, `6`, `9`), Google Snappy 1.2.2 (`1`, `2`), LZ4 1.10.0 (default fast and HC level 9), and a memcpy baseline. It does not change production dependencies, PTY messages, encryption, transport policy, or deployment.

The four-host Linux experiment uses the same frozen corpus and records speed and CPU cost independently on each machine. See [static Linux build instructions](BUILD_LINUX.md). Its schema-2 harness recreates codec contexts outside each timed stage and warms them before measuring; only one context is retained at a time to fit small VPS memory. The older Mac report retains its original schema-1 measurements and source hashes; it was not silently replaced with a new run.

Measured reports: [three actual VPS hosts plus OpenWrt, emphasizing speed and CPU](results/2026-09-11-linux-fleet/REPORT.zh-CN.md), and the [original M4 Max baseline](results/2026-09-11-m4-max/REPORT.zh-CN.md). The fleet report includes all 800 host/dataset/configuration combinations and raw rounds. Regenerate tables with `python3 tools/compression-bench/analyze_fleet.py tools/compression-bench/results/2026-09-11-linux-fleet`; add `--plots` with matplotlib installed to regenerate figures.

The Rust benchmark is `src/main.rs`. The small `native.cc` bridge is necessary to select **official Snappy Level 2**, rather than substituting a Rust implementation with different compression behavior. Native versions are compile-time asserted and source archives are SHA-256 pinned. Upstream licenses remain in the extracted upstream sources; this experiment does not vendor or distribute their source archives/binaries.

## Run

Requires Rust, Python 3.12+, C/C++ compiler, CMake and `ar` on macOS or Linux. The native preparation downloads the three pinned source archives from upstream GitHub. Build output can live in a temporary directory:

```sh
python3 tools/compression-bench/prepare_native.py /tmp/flowsplice-compression-native
python3 tools/compression-bench/run_benchmark.py \
  /tmp/flowsplice-compression-native \
  tools/compression-bench/results/my-host \
  --rounds 5 --sample-ms 120
```

`run_benchmark.py` performs Release correctness tests and builds before timing; it records source/binary hashes, native compiler commands, host metadata, raw measurements and summary CSV. Run it after other builds finish. The benchmark itself has one thread; no background system settings are changed. A full run takes roughly 4–6 minutes, depending on machine speed. Native archives and Cargo build output are ignored; result files are intentionally reviewable.

To freeze data for remote testing, use the built Rust executable with `--export-bundle REPO frozen-corpus.bin`. Run a target executable with `--bundle frozen-corpus.bin OUTPUT 5 120`. Dataset hashes in every result must match the frozen baseline. `run_on_host.sh` runs in a caller-supplied dedicated temporary directory, records host/process observations, uses niceness 10 with a 256 MiB address-space ceiling and 900-second timeout, and never installs software or restarts services. Actual SSH endpoints and full service snapshots must remain private, outside public results.

For only the correctness checks:

```sh
BENCH_NATIVE_DIR=/tmp/flowsplice-compression-native \
  cargo test --release --locked --manifest-path tools/compression-bench/Cargo.toml
```

## What is measured

- 20 deterministic corpus groups, described in [CORPUS.md](CORPUS.md). Repository inputs come from the recorded checkout; generated inputs are explicitly labeled synthetic. No production terminal history is collected.
- Every block is an independent compression unit. No persistent dictionary, training, or cross-block history. Output buffers and codec contexts are reused after warmup. Internal codec work, including internal allocations where applicable, remains timed.
- Five rounds, with all dataset/algorithm/direction stages shuffled independently using a recorded fixed seed. Each stage runs for at least 120 ms. The summary uses per-case medians, with minimum/maximum speeds retained.
- Compression and decompression throughput both use **original, uncompressed MiB per wall second**. 1 MiB = 1,048,576 bytes.
- Compressed size is the upstream library output. Zstd uses an independent frame, Snappy raw includes its own size prefix, LZ4 uses raw blocks with original sizes known externally. Application envelope, length metadata, TLS overhead, serialization, network, disk, UI rendering and cold initialization are **not included**. This is a codec comparison, not an end-to-end latency measurement.
- CPU occupancy is `process(user + system) CPU time / wall time × 100`, using `getrusage`. **100% means one fully occupied logical core**, not the entire machine. These single-threaded saturation tests usually have similar occupancy. `CPU ms / original MiB` is the useful work-cost comparison. CPU occupancy is not battery power or energy consumption.
- p50/p95/max per-block warm latency comes from 512 additional timer probes after throughput measurements. These probes include timer/call overhead and are descriptive microbenchmark results, not network tail-latency guarantees.
- Every dataset/algorithm block is verified byte-for-byte before timing and again afterward. Tests also cover empty input, size boundaries, exact raw/JSON pairing, determinism and declared library versions.
- `none-copy` measures the same bridge with a memory copy; it is a reference overhead floor, not a claim that uncompressed networking needs precisely one copy.

Do not interpret template-heavy generated history/prose as natural production traffic, or average all inputs into an assumed real-world workload. Both incompressible controls and genuine repository text are included to expose this limitation. Four Linux hosts now have direct measurements; other machines and mobile devices still require their own runtime measurements. Neither Mac nor OpenWrt performance substitutes for iPad/Android results.

## Official references

- [Snappy 1.2.0 Level API release](https://github.com/google/snappy/releases/tag/1.2.0)
- [Snappy 1.2.2 options and experimental Level 2 annotation](https://github.com/google/snappy/blob/1.2.2/snappy.h)
- [Zstd 1.5.7 API](https://github.com/facebook/zstd/blob/v1.5.7/lib/zstd.h)
- [LZ4 1.10.0 raw-block API](https://github.com/lz4/lz4/blob/v1.10.0/lib/lz4.h)
- [LZ4 1.10.0 HC API](https://github.com/lz4/lz4/blob/v1.10.0/lib/lz4hc.h)
