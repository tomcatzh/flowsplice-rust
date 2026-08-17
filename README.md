# FlowSplice

FlowSplice is a small, identity-aware private service access system. It exposes explicitly configured home TCP and UDP services to an enrolled travel device without giving the relay or central coordinator access to business plaintext.

This repository is the Rust implementation. It is one Cargo workspace and one Git repository; it does not reuse the previous Go implementation.

## Components

| Package | Role |
| --- | --- |
| `flowsplice-server` | Home-side controller, service-catalog authority, and opaque work-socket coordinator. |
| `flowsplice-relay` | Public management/data ingress and Linux `splice(2)` opaque forwarding. |
| `flowsplice-homeagent` | Publishes configured services, terminates business TLS, and connects flows to home targets. |
| `flowsplice-travelagent` | Creates local TCP/UDP mappings, originates business TLS, and serves the embedded TypeScript UI. |
| `flowsplice-core` | Shared protocol framing, route-ticket authentication, TLS identity, and configuration support. |

The management plane uses mutual TLS. Every leaf certificate contains exactly one URI SAN in the form `flowsplice://identity/<role>/<id>`. Management and business traffic use separate CA roots, and every peer set has a mandatory SHA-256 SPKI allowlist. Business TLS is terminated only by Travel and Home; Relay and Server see an opaque byte stream.

See [Architecture](docs/architecture.md) for the detailed boundary and protocol flow.

## Repository layout

```text
crates/flowsplice-core/   shared Rust crate
server/                   independent server application
relay/                    independent relay application
homeagent/                independent home agent application
travelagent/              independent travel agent + TypeScript UI
tests/                    shared fixtures and Docker E2E suite
docker/                   E2E and static-release builders
scripts/                  release orchestration
```

## Cryptography and frontend

- TLS is provided by [rustls](https://docs.rs/rustls/) with the AWS-LC provider explicitly installed at process startup.
- Route-ticket HMAC and random secrets use `aws-lc-rs` directly.
- The Travel Agent UI is TypeScript built with Vite, precompressed at build time, and embedded with [embedded-spa v0.1.1](https://github.com/tomcatzh/embedded-spa/tree/v0.1.1).
- API routes are isolated from SPA fallback; missing API routes and hashed assets return real `404` responses.

## Build and check

Requirements: stable Rust, Node.js/npm, CMake, Clang, and Perl. Docker is additionally required for E2E and Linux release artifacts.

```bash
make check
make test
make e2e
```

`make e2e` generates two temporary test CAs, builds the Linux applications, starts all four components plus TCP/UDP echo targets, and validates:

- TCP and UDP data through the complete topology;
- mutual management TLS and separate Travel-to-Home business TLS;
- single-use HMAC-authenticated route setup;
- the embedded UI, gzip/Brotli selection, representation-specific ETags, and correct `404` boundaries.

Generated keys, web output, build targets, and release binaries are ignored by Git.

## Configuration

Every executable accepts `--config <path>` or `FLOWSPLICE_CONFIG`. Example files live beside each application:

- [server/config.example.toml](server/config.example.toml)
- [relay/config.example.toml](relay/config.example.toml)
- [homeagent/config.example.toml](homeagent/config.example.toml)
- [travelagent/config.example.toml](travelagent/config.example.toml)

The provided certificate generator is for disposable E2E testing only. Production deployments must provision and protect their own management and business CAs, leaf keys, certificate renewal, and SPKI allowlists. Startup fails when a required allowlist is empty or malformed.

The Travel UI and local mappings bind to loopback by default. A non-loopback UI requires `allow_remote_listen = true` and an administrator bearer token of at least 32 characters.

## Release artifacts

```bash
./scripts/build-release.sh
```

This produces four executables under each of:

- `dist/linux-amd64/` — static PIE, musl;
- `dist/linux-arm64/` — statically linked, musl;
- `dist/macos-arm64/` — self-contained arm64 Mach-O executables.

Linux artifacts are genuinely static. macOS does not support fully static linkage of Apple system libraries; the macOS deliverables are single executables with all FlowSplice code and web assets embedded.

## Current scope

The first release supports one active Home Agent, a canonical catalog, TCP streams, and UDP associations. Each accepted local connection/association receives an independent end-to-end TLS Carrier. Automated enrollment, certificate rotation/revocation, multi-home routing, cross-process session resume, multiple Carriers per logical session, and per-Travel service ACLs remain explicit later protocol work.

## License

[MIT](LICENSE)
