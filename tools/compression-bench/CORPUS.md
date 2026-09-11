# Compression benchmark corpus

`src/corpus.rs::build(repo)` prepares 20 deterministic datasets before any codec timing. Each generated dataset has 16 distinct blocks. Every block is compressed independently: there is no shared dictionary or streaming state. Sizes below are uncompressed bytes, not character counts. There are no captured production sessions, personal shell histories, tmux buffers, configuration files, certificates, credentials, or user data.

| Dataset | Contents |
| --- | --- |
| `input-1b`, `input-8b`, `input-64b` | Fictional keystrokes and varied command fragments of exactly 1, 8, and 64 bytes. |
| `shell-512b`, `shell-4k` | Varied generated compiler/test and shell-style output, exactly 512 / 4096 bytes. |
| `ansi-4k` | Generated terminal process-table updates with cursor positioning, color changes, and erase-to-line-end sequences, exactly 4096 bytes. |
| `shell-4k-output-json`, `ansi-4k-output-json` | **Exactly the same blocks** as their named raw counterparts, serialized as current `ServerMessage::Output` JSON with numeric byte arrays. Fixed non-nil fixture attachment UUID. |
| `history-256-ordinary` | 256 varied short log rows per `Reply::History` JSON body; 16 consecutive pages of a fictional 4096-row capture. |
| `history-256-near96k` | 256 varied 378-byte ASCII log rows per body. The serialized `lines` array is 97,537 bytes, below the actual 98,304-byte protocol page limit; full body is slightly larger than the array. |
| `repo-rust-16k`, `repo-rust-96k` | Genuine `.rs` files under `crates/`, sorted by path, concatenated once, then naturally chunked at 16 / 96 KiB. No file repetition or padding. |
| `repo-markdown-16k`, `repo-markdown-96k` | Genuine English-oriented repository documentation, explicit allowlist below, concatenated once and naturally chunked. May contain isolated non-English examples. |
| `english-16k` | Generated English technical prose with varied topic words, sequence numbers, record counts and timings. |
| `mixed-chinese-english-16k` | Generated Chinese technical prose and English annotations with varied counters and states. |
| `structured-logs-4k` | Generated JSON-line-style records with varied fictional timestamps, components, IDs, counters and severity. |
| `pseudorandom-4k`, `pseudorandom-96k` | Fixed-seed xorshift64 byte stream, an incompressibility control, not a cryptographic RNG. |
| `repo-png-16k` | Genuine already-compressed `assets/brand/pty/pty-icon.png`, naturally split into 16 KiB chunks. Individual chunks are not complete PNG images. |

## Provenance and bounds

The markdown allowlist is `README.md`, `docs/architecture.md`, `docs/cryptography.md`, `docs/socket-runtime.md`, `docs/pty.md`, `pty-apple/README.md`, `pty-android/README.md`, and `openwrt/README.md`. No broad documentation/home-directory scan is performed. Rust discovery only visits `crates/` and does not follow symlinks. Actual source paths are recorded in each dataset's provenance. Each concatenated repository corpus and PNG is bounded at 16 MiB; missing allowlisted inputs fail rather than silently substituting synthetic bytes. Final repository chunks retain their natural short size. In particular, the 96 KiB documentation corpus may have fewer than eight blocks: the real documents are not repeated to manufacture sample count.

The same checkout yields identical bytes; repository datasets intentionally change when those source files change. Generated terminal/text/log records use varied templates rather than repeating one short string to pad a block. Byte-oriented cuts can split lines, ANSI sequences, JSON-line records, or multibyte UTF-8, as transport reads can. History rows themselves are valid strings without newlines, and their JSON is complete.

The JSON shapes and field order mirror `crates/flowsplice-pty-protocol/src/lib.rs`: output is `type`, `attachment_id`, `data`; history is `status`, `attachment_id`, `capture_id`, `total_lines`, `start`, `columns`, `lines`. History includes neither the outer response/request-ID envelope nor a framing prefix. The 96 KiB protocol bound applies to the serialized **lines array**, not the complete history body. Output raw/JSON pairs permit serialization-overhead comparison, but a JSON codec ratio uses the JSON byte length as its denominator; it is not directly the raw terminal-wire saving.

## Interpretation limits

This is a reproducible microbenchmark mix, **not measured production traffic**. Synthetic terminal text and prose have template-induced redundancy and cannot establish a real workload distribution. PNG/random controls and genuine repository source/docs provide contrasting inputs, but no single aggregate should be treated as a production-weighted ratio. Tiny-input expansion and call overhead matter separately from large-buffer throughput. Dataset construction, disk reads and JSON encoding must remain outside codec timings.
