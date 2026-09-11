//! Deterministic, secret-free benchmark inputs; preparation is not timed.
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

pub struct Dataset {
    pub name: String,
    pub description: String,
    pub provenance: String,
    pub blocks: Vec<Vec<u8>>,
}

const ID: &str = "11111111-1111-4111-8111-111111111111";
const CAPTURE: &str = "22222222-2222-4222-8222-222222222222";
const N: usize = 16;

fn dataset(name: &str, description: &str, provenance: &str, blocks: Vec<Vec<u8>>) -> Dataset {
    Dataset {
        name: name.into(),
        description: description.into(),
        provenance: provenance.into(),
        blocks,
    }
}

fn generated(size: usize, kind: usize) -> Vec<Vec<u8>> {
    (0..N).map(|block| {
        let mut text = String::new();
        let mut row = 0usize;
        while text.len() < size {
            let n = block * 10007 + row;
            let line = match kind {
                0 => format!("dev@fixture:/workspace/project$ cargo test module_{n}\r\n   Compiling component_{} v0.{}.{}\r\ntest case_{n} ... {} ({} ms)\r\n", n % 23, n % 8, n % 17, if n.is_multiple_of(19) { "ignored" } else { "ok" }, n % 137),
                1 => format!("\x1b[{};1H\x1b[{}m{:5} worker-{:03} cpu {:2}.{:01}% memory {} KiB status {}\x1b[0m\x1b[K\r\n", row % 48 + 1, 31 + n % 7, n, n % 311, n % 99, n % 10, 1024 + n % 65000, ["running", "sleeping", "waiting", "ready"][n % 4]),
                2 => format!("Section {n}: The {} service processes {} records through a bounded queue. Each request retains its sequence number; retries preserve ordering while observers measure {} microseconds of latency.\n", ["archive", "terminal", "routing", "storage", "metrics"][n % 5], n % 901 + 1, n % 1103),
                3 => format!("第{n}节：{}服务完成{}条记录，终端会话保持顺序；网络恢复后继续处理队列，观察延迟为{}微秒。English annotation: batch {}, state {}.\n", ["存储", "终端", "路由", "监控", "归档"][n % 5], n % 901 + 1, n % 1103, n % 101, ["ready", "busy", "idle"][n % 3]),
                _ => format!("{{\"timestamp\":\"2026-01-01T{:02}:{:02}:{:02}Z\",\"level\":\"{}\",\"component\":\"worker_{}\",\"request_id\":\"fixture-{n:08x}\",\"bytes\":{},\"elapsed_us\":{},\"message\":\"batch processed\"}}\n", n / 3600 % 24, n / 60 % 60, n % 60, if n.is_multiple_of(13) { "WARN" } else { "INFO" }, n % 17, n * 31 % 65536, n % 739),
            };
            text.push_str(&line);
            row += 1;
        }
        // Blocks model byte-oriented transport cuts, including partial UTF-8 sequences.
        text.into_bytes()[..size].to_vec()
    }).collect()
}

fn history(wide: bool) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
    (0..N).map(|page| {
        let lines: Vec<String> = (0..256).map(|row| {
            let n = page * 256 + row;
            let mut line = format!("2026-01-01 12:{:02}:{:02} worker-{} processed batch {n}, records={}, elapsed={}ms", n / 60 % 60, n % 60, n % 17, n % 997, n % 131);
            if wide {
                let mut field = 0;
                while line.len() < 378 {
                    line.push_str(&format!(" shard_{}=partition-{:04}:{}", field, (n * 31 + field * 17) % 10000, ["ready", "queued", "done"][ (n + field) % 3]));
                    field += 1;
                }
                line.truncate(378);
            }
            line
        }).collect();
        let encoded = serde_json::to_string(&lines)?;
        assert!(encoded.len() <= 96 * 1024);
        // Match Reply::History field order as well as its tagged JSON schema.
        Ok(format!("{{\"status\":\"history\",\"attachment_id\":\"{ID}\",\"capture_id\":\"{CAPTURE}\",\"total_lines\":4096,\"start\":{},\"columns\":{},\"lines\":{encoded}}}", page * 256, if wide { 400 } else { 120 }).into_bytes())
    }).collect()
}

fn rust_paths(dir: &Path, paths: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            rust_paths(&entry.path(), paths)?;
        } else if ty.is_file() && entry.path().extension().is_some_and(|e| e == "rs") {
            paths.push(entry.path());
        }
    }
    Ok(())
}

pub fn build(repo: &Path) -> Result<Vec<Dataset>, Box<dyn Error>> {
    let mut result = Vec::new();
    for size in [1, 8, 64] {
        let blocks = (0..N)
            .map(|i| {
                let command = format!(
                    "cargo test --package component_{i:02} --test scenario_{} -- --nocapture\r",
                    i * 7
                );
                if size == 1 {
                    vec![b"asdfjkl;\r\t\x1b qwer"[i % 16]]
                } else if size == 8 {
                    format!("ls -l {i:x}\r").into_bytes()
                } else {
                    command.into_bytes()[..size].to_vec()
                }
            })
            .collect();
        result.push(dataset(
            &format!("input-{size}b"),
            "Synthetic interactive input fragments",
            "Deterministic fictional command input; not captured keystrokes",
            blocks,
        ));
    }
    for (name, size, kind) in [
        ("shell-512b", 512, 0),
        ("shell-4k", 4096, 0),
        ("ansi-4k", 4096, 1),
    ] {
        let blocks = generated(size, kind);
        if size == 4096 {
            let json = blocks
                .iter()
                .map(|b| {
                    Ok(format!(
                        "{{\"type\":\"output\",\"attachment_id\":\"{ID}\",\"data\":{}}}",
                        serde_json::to_string(b)?
                    )
                    .into_bytes())
                })
                .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
            result.push(dataset(&format!("{name}-output-json"), "ServerMessage::Output JSON body, paired byte-for-byte with raw dataset", "Synthetic raw bytes encoded using current PTY numeric-array schema; no frame prefix", json));
        }
        result.push(dataset(
            name,
            "Synthetic terminal output byte chunks",
            "Generated varied fictional shell output / ANSI cursor and color updates",
            blocks,
        ));
    }
    for wide in [false, true] {
        result.push(dataset(if wide { "history-256-near96k" } else { "history-256-ordinary" }, "Reply::History JSON bodies, 256 rows per page", "Synthetic log rows; current protocol fields; excludes ServerMessage::Response envelope and framing", history(wide)?));
    }
    let mut paths = Vec::new();
    rust_paths(&repo.join("crates"), &mut paths)?;
    paths.sort();
    let markdown = [
        "README.md",
        "docs/architecture.md",
        "docs/cryptography.md",
        "docs/socket-runtime.md",
        "docs/pty.md",
        "pty-apple/README.md",
        "pty-android/README.md",
        "openwrt/README.md",
    ];
    for (label, files) in [
        ("rust", paths),
        ("markdown", markdown.iter().map(|p| repo.join(p)).collect()),
    ] {
        let mut bytes = Vec::new();
        let mut names = Vec::new();
        for path in files {
            names.push(path.strip_prefix(repo)?.display().to_string());
            let data = fs::read(&path)?;
            if bytes.len() + data.len() > 16 * 1024 * 1024 {
                return Err("repository corpus exceeds 16 MiB bound".into());
            }
            bytes.extend(data);
        }
        for size in [16384, 98304] {
            result.push(dataset(&format!("repo-{label}-{}k", size / 1024), "Natural contiguous chunks of concatenated repository files; final chunk may be short", &format!("Genuine checked-out repository content (no padding), ordered files: {}", names.join(", ")), bytes.chunks(size).map(<[u8]>::to_vec).collect()));
        }
    }
    for (name, kind) in [
        ("english-16k", 2),
        ("mixed-chinese-english-16k", 3),
        ("structured-logs-4k", 4),
    ] {
        result.push(dataset(name, "Deterministic varied generated text", "Synthetic templates with varying records, counters and vocabulary; not a natural-language benchmark collection", generated(if kind == 4 { 4096 } else { 16384 }, kind)));
    }
    for size in [4096, 98304] {
        let mut state = 0x9e3779b97f4a7c15u64;
        let blocks = (0..N)
            .map(|_| {
                (0..size)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        (state >> 32) as u8
                    })
                    .collect()
            })
            .collect();
        result.push(dataset(
            &format!("pseudorandom-{}k", size / 1024),
            "Fixed-seed xorshift64 bytes, incompressibility control",
            "Synthetic non-cryptographic deterministic pseudorandom stream",
            blocks,
        ));
    }
    let png_path = "assets/brand/pty/pty-icon.png";
    let png = fs::read(repo.join(png_path))?;
    if png.len() > 16 * 1024 * 1024 || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("invalid or oversized repository PNG".into());
    }
    result.push(dataset(
        "repo-png-16k",
        "Already-compressed PNG asset, natural chunks (including short tail)",
        &format!("Genuine repository asset: {png_path}; chunks are not standalone PNG files"),
        png.chunks(16384).map(<[u8]>::to_vec).collect(),
    ));
    Ok(result)
}
