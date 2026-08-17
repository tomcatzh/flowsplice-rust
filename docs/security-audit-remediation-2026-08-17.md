# Security Audit Remediation — 2026-08-17

This document records the repository-side verification of the user-supplied Kimi K3 read-only audit. It is a remediation record, not a professional penetration-test report or certification.

## Disposition

| Finding | Verification | Disposition |
| --- | --- | --- |
| H1 — unbounded frame reads / slow-loris | Confirmed. The former `read_exact`-based helper also lost partial framing state when cancelled inside a repeated `tokio::select!`. | Replaced by a persistent stateful decoder built from cancellation-safe `read` calls. HELLO, registration, catalog, route-response, TLS setup, and business `OPEN` reads now have deadlines. Unit and Docker slow-frame regressions cover the failure. |
| H2 — UDP association queue can block the shared listener | Confirmed. Awaiting a full per-peer channel stopped ingress for every peer. | The listener now uses `try_send`; saturation drops only the current datagram and a closed association is removed safely. |
| H3 — catalog polling creates a full mTLS connection every five seconds | Confirmed, including the missing Relay-to-Travel push path. | Travel now keeps one authenticated catalog subscription. Relay uses a watch channel to push every catalog replacement. Docker E2E restarts Home with a changed catalog and proves that Travel receives it without reconnecting. |
| M1 — Home session can be silently replaced and is not pinned by Server | Confirmed. | Server now requires `home_id` plus a non-empty Home management SPKI allowlist. A valid replacement explicitly closes the old session and logs the takeover. Relay applies the same explicit supersession discipline to Server sessions. |
| M2 — restart loses in-memory route/work secrets | Confirmed as a documented availability boundary. | No code change. Cross-process continuity requires a separate persistence/resume design and must not be implied by the current protocol. |
| M3 — secrets are JSON byte arrays and are not guaranteed to be zeroized | Confirmed as protocol and memory-hygiene debt, not an immediate authentication break. | Deferred to a versioned protocol change. The README continues to disclose cloning, representation, and zeroization limits. |
| M4 — `rustls-pemfile` is unmaintained | Confirmed by RUSTSEC-2025-0134. | Removed. PEM loading now uses `rustls-pki-types`; `cargo audit` reports no vulnerability or warning. |
| M5 — UDP association-map growth | The proposed unbounded-growth mechanism is not present: a permit is acquired before insertion and every inserted association owns one permit. | No redundant second limit added. H2 removes the cross-peer blocking failure while the existing active-flow ceiling remains authoritative. |
| L1 — TLS 1.2 is enabled | Confirmed. | Removed the `tls12` features and explicitly configured every rustls client/server builder for TLS 1.3 only. Docker E2E proves TLS 1.2 rejection and TLS 1.3 success. |
| L2 — heartbeats do not enforce liveness | Confirmed. | Established management links now expire after 30 seconds without a complete inbound message and reconnect through their existing outer loops. |
| L3 — bearer comparison is not constant-time | Confirmed. | Remote UI bearer comparison now uses AWS-LC's constant-time slice verification. |
| L4 — UDP buffer allocation | No defect found; the buffers were already allocated once outside their loops and frame bounds precede payload validation. | No change. |
| L5 — fixed E2E token is shorter than the minimum | Not reproduced; the test token satisfies the configured minimum and the E2E topology passes startup validation. | No change. Production documentation still requires a high-entropy token. |
| L6 — TLS configuration and PEM files are rebuilt per flow | Confirmed in Home and Travel hot paths. | Connectors and acceptors are built once at process startup and reused. Certificate rotation is deliberately restart-scoped. |

## Verification

- `cargo test --workspace --all-targets`: 7 tests passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo audit`: zero vulnerabilities and zero warnings.
- Docker E2E: initial TCP/UDP, embedded SPA, TLS-1.3-only policy, incomplete-frame expiry, Home restart, live catalog push, and post-restart TCP/UDP all passed.
- Release build: macOS arm64 plus static Linux amd64 and arm64 versions of all four executables completed.

## Primary references

- [Tokio `select!` cancellation safety](https://docs.rs/tokio/latest/tokio/macro.select.html)
- [Tokio `mpsc::Sender::try_send`](https://docs.rs/tokio/latest/tokio/sync/mpsc/struct.Sender.html#method.try_send)
- [rustls `ConfigBuilder`](https://docs.rs/rustls/latest/rustls/struct.ConfigBuilder.html)
- [RUSTSEC-2025-0134](https://rustsec.org/advisories/RUSTSEC-2025-0134.html)
