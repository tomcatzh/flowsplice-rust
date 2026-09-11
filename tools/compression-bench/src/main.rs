//! Reproducible, single-threaded, independent-block codec experiment.
//! This binary is deliberately outside the production Cargo workspace.
mod corpus;
mod frozen;

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    ffi::{CStr, c_char, c_int, c_void},
    fs,
    hint::black_box,
    io::Write,
    path::{Path, PathBuf},
    ptr::NonNull,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

unsafe extern "C" {
    fn bench_new(kind: c_int) -> *mut c_void;
    fn bench_free(context: *mut c_void);
    fn bench_bound(kind: c_int, length: usize) -> usize;
    fn bench_compress(
        context: *mut c_void,
        level: c_int,
        src: *const u8,
        length: usize,
        dst: *mut u8,
        capacity: usize,
    ) -> usize;
    fn bench_decompress(
        context: *mut c_void,
        src: *const u8,
        length: usize,
        dst: *mut u8,
        capacity: usize,
    ) -> usize;
    fn bench_versions() -> *const c_char;
}

#[derive(Clone, Copy, Serialize)]
struct Algorithm {
    name: &'static str,
    kind: i32,
    level: i32,
}
const ALGORITHMS: [Algorithm; 10] = [
    Algorithm {
        name: "none-copy",
        kind: 0,
        level: 0,
    },
    Algorithm {
        name: "zstd-fast3",
        kind: 1,
        level: -3,
    },
    Algorithm {
        name: "zstd-1",
        kind: 1,
        level: 1,
    },
    Algorithm {
        name: "zstd-3",
        kind: 1,
        level: 3,
    },
    Algorithm {
        name: "zstd-6",
        kind: 1,
        level: 6,
    },
    Algorithm {
        name: "zstd-9",
        kind: 1,
        level: 9,
    },
    Algorithm {
        name: "snappy-1",
        kind: 2,
        level: 1,
    },
    Algorithm {
        name: "snappy-2",
        kind: 2,
        level: 2,
    },
    Algorithm {
        name: "lz4-fast",
        kind: 3,
        level: 1,
    },
    Algorithm {
        name: "lz4-hc9",
        kind: 4,
        level: 9,
    },
];

struct Codec {
    algorithm: Algorithm,
    context: NonNull<c_void>,
}
impl Codec {
    fn new(algorithm: Algorithm) -> Self {
        // The native context is owned by this single-threaded Codec until Drop.
        let context = NonNull::new(unsafe { bench_new(algorithm.kind) }).expect("codec allocation");
        Self { algorithm, context }
    }
    fn bound(&self, length: usize) -> usize {
        let bound = unsafe { bench_bound(self.algorithm.kind, length) };
        assert_ne!(bound, usize::MAX);
        bound
    }
    fn compress(&mut self, input: &[u8], output: &mut [u8]) -> usize {
        assert!(output.len() >= self.bound(input.len()));
        // Disjoint Rust slices remain alive through the call; native code checks lengths.
        let size = unsafe {
            bench_compress(
                self.context.as_ptr(),
                self.algorithm.level,
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
                output.len(),
            )
        };
        assert!(
            size <= output.len(),
            "{} compression failed",
            self.algorithm.name
        );
        black_box(size)
    }
    fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> usize {
        let size = unsafe {
            bench_decompress(
                self.context.as_ptr(),
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
                output.len(),
            )
        };
        assert_eq!(
            size,
            output.len(),
            "{} decompression failed",
            self.algorithm.name
        );
        black_box(size)
    }
}
impl Drop for Codec {
    fn drop(&mut self) {
        unsafe { bench_free(self.context.as_ptr()) }
    }
}

fn cpu_seconds() -> f64 {
    // RUSAGE_SELF counts only this process's user+system time, not other host work.
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
}

#[derive(Serialize, Clone)]
struct Sample {
    round: usize,
    operations: u64,
    original_bytes_processed: u64,
    wall_seconds: f64,
    cpu_seconds: f64,
    mib_per_second: f64,
    cpu_percent_one_core: f64,
    cpu_ms_per_mib: f64,
}

fn measure(
    codec: &mut Codec,
    raw: &[Vec<u8>],
    packed: &[Vec<u8>],
    compress: bool,
    duration: Duration,
    round: usize,
) -> Sample {
    // Allocation is outside timing. Each call still produces an independent block.
    // Context reuse does not retain a compression dictionary between blocks.
    let max_raw = raw.iter().map(Vec::len).max().unwrap();
    let max_bound = raw.iter().map(|b| codec.bound(b.len())).max().unwrap();
    let mut output = vec![0u8; if compress { max_bound } else { max_raw }];
    let run = |codec: &mut Codec, index: usize, output: &mut [u8]| {
        let size = if compress {
            codec.compress(black_box(&raw[index]), output)
        } else {
            codec.decompress(black_box(&packed[index]), &mut output[..raw[index].len()])
        };
        if size > 0 {
            black_box(output[0]);
            black_box(output[size - 1]);
        }
    };
    for index in 0..raw.len() {
        run(codec, index, &mut output);
    }
    let batch = (64 * 1024 / max_raw.max(1)).clamp(1, 256);
    let mut operations = 0u64;
    let mut bytes = 0u64;
    let mut index = 0usize;
    let start = Instant::now();
    let cpu_start = cpu_seconds();
    loop {
        for _ in 0..batch {
            run(codec, index, &mut output);
            bytes += raw[index].len() as u64;
            operations += 1;
            index += 1;
            if index == raw.len() {
                index = 0;
            }
        }
        if start.elapsed() >= duration {
            break;
        }
    }
    let cpu = (cpu_seconds() - cpu_start).max(0.0);
    let wall = start.elapsed().as_secs_f64();
    let mib = bytes as f64 / 1_048_576.0;
    Sample {
        round,
        operations,
        original_bytes_processed: bytes,
        wall_seconds: wall,
        cpu_seconds: cpu,
        mib_per_second: mib / wall,
        cpu_percent_one_core: cpu / wall * 100.0,
        cpu_ms_per_mib: cpu * 1000.0 / mib,
    }
}

#[derive(Serialize)]
struct Latency {
    p50_us: f64,
    p95_us: f64,
    max_us: f64,
    count: usize,
}
fn latency(codec: &mut Codec, raw: &[Vec<u8>], packed: &[Vec<u8>], compress: bool) -> Latency {
    let max = raw.iter().map(|b| codec.bound(b.len())).max().unwrap();
    let mut output = vec![0u8; max];
    let mut times = Vec::new();
    for i in 0..512 {
        let index = i % raw.len();
        let start = Instant::now();
        let size = if compress {
            codec.compress(black_box(&raw[index]), &mut output)
        } else {
            codec.decompress(black_box(&packed[index]), &mut output[..raw[index].len()])
        };
        let elapsed = start.elapsed().as_secs_f64() * 1e6;
        black_box(&output[..size]);
        times.push(elapsed);
    }
    times.sort_by(f64::total_cmp);
    Latency {
        p50_us: quantile(&times, 0.5),
        p95_us: quantile(&times, 0.95),
        max_us: *times.last().unwrap(),
        count: times.len(),
    }
}
fn quantile(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() as f64 * q).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1)]
}
fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut values = values.collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    quantile(&values, 0.5)
}

#[derive(Serialize)]
struct Stats {
    median_mib_per_second: f64,
    min_mib_per_second: f64,
    max_mib_per_second: f64,
    median_cpu_percent_one_core: f64,
    median_cpu_ms_per_mib: f64,
}
fn stats(samples: &[Sample]) -> Stats {
    Stats {
        median_mib_per_second: median(samples.iter().map(|x| x.mib_per_second)),
        min_mib_per_second: samples
            .iter()
            .map(|x| x.mib_per_second)
            .fold(f64::INFINITY, f64::min),
        max_mib_per_second: samples.iter().map(|x| x.mib_per_second).fold(0.0, f64::max),
        median_cpu_percent_one_core: median(samples.iter().map(|x| x.cpu_percent_one_core)),
        median_cpu_ms_per_mib: median(samples.iter().map(|x| x.cpu_ms_per_mib)),
    }
}

#[derive(Serialize)]
struct DatasetInfo {
    name: String,
    description: String,
    provenance: String,
    block_count: usize,
    original_bytes: usize,
    min_block_bytes: usize,
    max_block_bytes: usize,
    sha256_length_prefixed_blocks: String,
}
#[derive(Serialize)]
struct Case {
    dataset: String,
    algorithm: Algorithm,
    original_bytes: usize,
    compressed_bytes: usize,
    compressed_percent: f64,
    ratio_original_over_compressed: f64,
    expanded_blocks: usize,
    unchanged_blocks: usize,
    block_count: usize,
    compression_samples: Vec<Sample>,
    decompression_samples: Vec<Sample>,
    compression: Option<Stats>,
    decompression: Option<Stats>,
    compression_latency: Option<Latency>,
    decompression_latency: Option<Latency>,
}
#[derive(Serialize)]
struct Report {
    schema_version: u32,
    started_unix_seconds: u64,
    finished_unix_seconds: u64,
    reference_versions: String,
    rounds: usize,
    sample_ms: u64,
    shuffle_seed: u64,
    peak_rss_kib: u64,
    method: &'static str,
    datasets: Vec<DatasetInfo>,
    cases: Vec<Case>,
}

fn shuffled(count: usize, seed: &mut u64) -> Vec<usize> {
    let mut order = (0..count).collect::<Vec<_>>();
    for i in (1..count).rev() {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        order.swap(i, *seed as usize % (i + 1));
    }
    order
}
fn encode(codec: &mut Codec, blocks: &[Vec<u8>]) -> Vec<Vec<u8>> {
    blocks
        .iter()
        .map(|raw| {
            let mut packed = vec![0; codec.bound(raw.len())];
            let size = codec.compress(raw, &mut packed);
            packed.truncate(size);
            let mut decoded = vec![0; raw.len()];
            codec.decompress(&packed, &mut decoded);
            assert_eq!(raw, &decoded, "round-trip mismatch");
            packed
        })
        .collect()
}
fn checkpoint(path: &Path, report: &Report) -> Result<(), Box<dyn Error>> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(report)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}
fn unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|a| a == "--export-bundle") {
        if args.len() != 4 {
            return Err("Usage: --export-bundle REPO FILE".into());
        }
        let datasets = corpus::build(Path::new(&args[2]))?;
        frozen::write(Path::new(&args[3]), &datasets)?;
        eprintln!("Exported {} datasets", datasets.len());
        return Ok(());
    }
    if args.len() < 3 {
        eprintln!(
            "Usage: compression-bench REPO OUTPUT_DIR [ROUNDS=5] [SAMPLE_MS=120], or --bundle FILE OUTPUT_DIR [ROUNDS] [SAMPLE_MS]"
        );
        std::process::exit(2);
    }
    let (datasets, offset) = if args[1] == "--bundle" {
        if args.len() < 4 {
            return Err("missing bundle or output path".into());
        }
        (frozen::read(Path::new(&args[2]))?, 3)
    } else {
        (corpus::build(Path::new(&args[1]))?, 2)
    };
    let output = PathBuf::from(&args[offset]);
    let rounds: usize = args
        .get(offset + 1)
        .map(|x| x.parse())
        .transpose()?
        .unwrap_or(5);
    let sample_ms: u64 = args
        .get(offset + 2)
        .map(|x| x.parse())
        .transpose()?
        .unwrap_or(120);
    assert!(rounds > 0 && sample_ms > 0);
    fs::create_dir_all(&output)?;
    assert!(!datasets.is_empty());
    let infos = datasets
        .iter()
        .map(|d| {
            assert!(!d.blocks.is_empty() && d.blocks.iter().all(|b| !b.is_empty()));
            let mut digest = Sha256::new();
            for block in &d.blocks {
                digest.update((block.len() as u64).to_le_bytes());
                digest.update(block);
            }
            DatasetInfo {
                name: d.name.clone(),
                description: d.description.clone(),
                provenance: d.provenance.clone(),
                block_count: d.blocks.len(),
                original_bytes: d.blocks.iter().map(Vec::len).sum(),
                min_block_bytes: d.blocks.iter().map(Vec::len).min().unwrap(),
                max_block_bytes: d.blocks.iter().map(Vec::len).max().unwrap(),
                sha256_length_prefixed_blocks: format!("{:x}", digest.finalize()),
            }
        })
        .collect();
    let versions = unsafe { CStr::from_ptr(bench_versions()) }
        .to_str()?
        .to_owned();
    let mut report = Report {
        schema_version: 2,
        started_unix_seconds: unix(),
        finished_unix_seconds: 0,
        reference_versions: versions,
        rounds,
        sample_ms,
        shuffle_seed: 0x504_5459_2026,
        peak_rss_kib: 0,
        method: "single thread; warm in-memory independent blocks; preallocated output and reusable codec contexts within each timed stage, contexts recreated outside timing per stage to bound memory; no shared dictionary; raw reference-library output; no application/TLS framing, serialization, network or disk time; getrusage process user+system CPU, one core=100%; decompression throughput uses original bytes; medians over independently shuffled rounds; 512 separate warm block-latency probes after throughput tests",
        datasets: infos,
        cases: Vec::new(),
    };
    let mut prepared = Vec::new();
    for (di, dataset) in datasets.iter().enumerate() {
        for algorithm in ALGORITHMS {
            let mut codec = Codec::new(algorithm);
            let packed = encode(&mut codec, &dataset.blocks);
            let original_bytes: usize = dataset.blocks.iter().map(Vec::len).sum();
            let compressed_bytes: usize = packed.iter().map(Vec::len).sum();
            report.cases.push(Case {
                dataset: dataset.name.clone(),
                algorithm,
                original_bytes,
                compressed_bytes,
                compressed_percent: compressed_bytes as f64 / original_bytes as f64 * 100.0,
                ratio_original_over_compressed: original_bytes as f64 / compressed_bytes as f64,
                expanded_blocks: packed
                    .iter()
                    .zip(&dataset.blocks)
                    .filter(|(a, b)| a.len() > b.len())
                    .count(),
                unchanged_blocks: packed
                    .iter()
                    .zip(&dataset.blocks)
                    .filter(|(a, b)| a.len() == b.len())
                    .count(),
                block_count: packed.len(),
                compression_samples: Vec::new(),
                decompression_samples: Vec::new(),
                compression: None,
                decompression: None,
                compression_latency: None,
                decompression_latency: None,
            });
            prepared.push((di, algorithm, packed));
        }
    }
    eprintln!(
        "Validated {} datasets × {} algorithms = {} cases. {}",
        datasets.len(),
        ALGORITHMS.len(),
        prepared.len(),
        report.reference_versions
    );
    let path = output.join("results.json");
    checkpoint(&path, &report)?;
    let mut seed = report.shuffle_seed;
    for round in 0..rounds {
        let order = shuffled(prepared.len() * 2, &mut seed);
        for (step, task) in order.iter().enumerate() {
            let ci = task / 2;
            let compress = task % 2 == 0;
            let (di, algorithm, packed) = &mut prepared[ci];
            let mut codec = Codec::new(*algorithm);
            let sample = measure(
                &mut codec,
                &datasets[*di].blocks,
                packed,
                compress,
                Duration::from_millis(sample_ms),
                round,
            );
            if compress {
                report.cases[ci].compression_samples.push(sample);
            } else {
                report.cases[ci].decompression_samples.push(sample);
            }
            if (step + 1) % 100 == 0 {
                eprintln!(
                    "Round {}/{}: {}/{} stages",
                    round + 1,
                    rounds,
                    step + 1,
                    order.len()
                );
            }
        }
        checkpoint(&path, &report)?;
        eprintln!("Round {}/{} complete", round + 1, rounds);
    }
    for (ci, (di, algorithm, packed)) in prepared.iter_mut().enumerate() {
        let mut codec = Codec::new(*algorithm);
        assert_eq!(&encode(&mut codec, &datasets[*di].blocks), packed);
        let case = &mut report.cases[ci];
        case.compression = Some(stats(&case.compression_samples));
        case.decompression = Some(stats(&case.decompression_samples));
        case.compression_latency = Some(latency(&mut codec, &datasets[*di].blocks, packed, true));
        case.decompression_latency =
            Some(latency(&mut codec, &datasets[*di].blocks, packed, false));
        // Verify every block again after timed use of reused contexts.
        assert_eq!(&encode(&mut codec, &datasets[*di].blocks), packed);
    }
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let peak = unsafe { usage.assume_init() }.ru_maxrss as u64;
    report.peak_rss_kib = if cfg!(target_os = "macos") {
        peak / 1024
    } else {
        peak
    };
    report.finished_unix_seconds = unix();
    checkpoint(&path, &report)?;
    let mut csv = fs::File::create(output.join("summary.csv"))?;
    writeln!(
        csv,
        "dataset,algorithm,original_bytes,compressed_bytes,compressed_percent,ratio,compress_mib_s,decompress_mib_s,compress_cpu_pct,decompress_cpu_pct,compress_cpu_ms_mib,decompress_cpu_ms_mib,compress_p95_us,decompress_p95_us"
    )?;
    for c in &report.cases {
        let a = c.compression.as_ref().unwrap();
        let b = c.decompression.as_ref().unwrap();
        writeln!(
            csv,
            "{},{},{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}",
            c.dataset,
            c.algorithm.name,
            c.original_bytes,
            c.compressed_bytes,
            c.compressed_percent,
            c.ratio_original_over_compressed,
            a.median_mib_per_second,
            b.median_mib_per_second,
            a.median_cpu_percent_one_core,
            b.median_cpu_percent_one_core,
            a.median_cpu_ms_per_mib,
            b.median_cpu_ms_per_mib,
            c.compression_latency.as_ref().unwrap().p95_us,
            c.decompression_latency.as_ref().unwrap().p95_us
        )?;
    }
    eprintln!("Finished and revalidated all blocks: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_reference_codecs_roundtrip_boundary_sizes() {
        for algorithm in ALGORITHMS {
            let mut codec = Codec::new(algorithm);
            for n in [0, 1, 8, 64, 255, 4096, 65536, 98304, 131072] {
                let blocks = vec![(0..n).map(|i| (i * 17 + i / 7) as u8).collect()];
                encode(&mut codec, &blocks);
            }
        }
    }
    #[test]
    fn linked_versions_and_level_api_are_pinned() {
        let versions = unsafe { CStr::from_ptr(bench_versions()) }
            .to_str()
            .unwrap();
        assert_eq!(versions, "snappy=1.2.2;zstd=1.5.7;lz4=1.10.0");
    }
    #[test]
    fn deterministic_corpus_and_paired_wire_encoding() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let first = corpus::build(&repo).unwrap();
        let second = corpus::build(&repo).unwrap();
        assert_eq!(first.len(), second.len());
        for raw_name in ["shell-4k", "ansi-4k"] {
            let raw = first.iter().find(|d| d.name == raw_name).unwrap();
            let json = first
                .iter()
                .find(|d| d.name == format!("{raw_name}-output-json"))
                .unwrap();
            assert_eq!(raw.blocks.len(), json.blocks.len());
            for (a, b) in raw.blocks.iter().zip(&json.blocks) {
                let message: serde_json::Value = serde_json::from_slice(b).unwrap();
                assert_eq!(message["type"], "output");
                let decoded: Vec<u8> = serde_json::from_value(message["data"].clone()).unwrap();
                assert_eq!(&decoded, a);
            }
        }
        for dataset in first.iter().filter(|d| d.name.starts_with("history-")) {
            for block in &dataset.blocks {
                let message: serde_json::Value = serde_json::from_slice(block).unwrap();
                assert_eq!(message["status"], "history");
                assert_eq!(message["lines"].as_array().unwrap().len(), 256);
                assert!(serde_json::to_vec(&message["lines"]).unwrap().len() <= 96 * 1024);
            }
        }
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.blocks, b.blocks);
            assert!(!a.blocks.is_empty());
            for algorithm in ALGORITHMS {
                encode(&mut Codec::new(algorithm), &a.blocks);
            }
        }
    }
}
