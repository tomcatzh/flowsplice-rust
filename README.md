# FlowSplice

FlowSplice is a small, identity-aware private service access system. It exposes explicitly configured home TCP and UDP services to an enrolled travel device without giving the relay or central coordinator access to business plaintext.

This repository is the Rust implementation. It is one Cargo workspace and one Git repository; it does not reuse the previous Go implementation.

## Components

| Package | Role |
| --- | --- |
| `flowsplice-server` | Home-side controller, service-catalog and Relay-directory authority, and opaque work-socket coordinator. |
| `flowsplice-relay` | Public management/data ingress and Linux `splice(2)` opaque forwarding. |
| `flowsplice-homeagent` | Publishes configured services, terminates business TLS, and connects flows to home targets. |
| `flowsplice-travelagent` | Creates local TCP/UDP mappings, originates business TLS, and serves the embedded TypeScript UI. |
| `flowsplice-foobar` | Low-rate single-TCP-connection loopback target and CLI continuity probe for deployment acceptance. |
| `flowsplice-core` | Shared protocol framing, route-ticket authentication, TLS identity, and configuration support. |

The management plane uses mutual TLS. Every leaf certificate contains exactly one FlowSplice URI SAN in the form `flowsplice://identity/<role>/<id>`. Management and business traffic use separate CA roots, and selected peer relationships are narrowed further with SHA-256 SPKI allowlists. Business TLS is terminated only by Travel and Home; Relay and Server forward its bytes without possessing the business private keys. Exact trust, visibility, and current limitations are documented below.

See [Architecture](docs/architecture.md) for the detailed boundary and protocol flow.
See [Security Audit Remediation — 2026-08-17](docs/security-audit-remediation-2026-08-17.md) for the finding-by-finding verification and disposition of the independent Kimi K3 review.

## Repository layout

```text
crates/flowsplice-core/   shared Rust crate
server/                   independent server application
relay/                    independent relay application
homeagent/                independent home agent application
travelagent/              independent travel agent + TypeScript UI
foobar/                   continuous loopback target and CLI probe
tests/                    shared fixtures and Docker E2E suite
docker/                   E2E and static-release builders
scripts/                  release orchestration
openwrt/                  UCI, procd, LuCI, and IPK package sources
```

## Security and cryptographic design

### Public design, secret keys

FlowSplice assumes that its source code, protocol, topology, algorithms, certificate profiles, and failure behavior are public. Its security must depend on private keys and freshly generated secrets, not on an attacker failing to learn how the system works. Publishing the construction makes the trust assumptions, metadata exposure, and operational duties reviewable. This section describes the current implementation; it is not a claim of a formal proof, independent audit, FIPS validation, or immunity to implementation bugs.

The primary goals are:

- authenticate every management peer before accepting control messages;
- authenticate Travel and Home to each other independently of Relay and Server;
- keep business plaintext and the selected service ID unavailable to Relay, Server, and passive network observers;
- prevent an unauthenticated socket from joining an allocated route merely by guessing an ID;
- constrain a management-key compromise from becoming a business endpoint credential;
- fail closed on malformed identity, certificate, pin, route-authentication, and bounded-frame checks.

FlowSplice does not attempt to hide IP addresses, connection timing, byte counts, TLS handshake metadata, the existence of a Home or Travel identity, or the service catalog from the control-plane components that distribute it. It also does not protect plaintext after Travel, Home, or the final local service endpoint is compromised.

### Cryptographic primitives and versions

- TLS is implemented by [`rustls`](https://docs.rs/rustls/latest/rustls/) through `tokio-rustls`. Cargo disables rustls default features and explicitly enables only the `aws_lc_rs` and `std` features.
- Every executable calls `rustls::crypto::aws_lc_rs::default_provider().install_default()` during startup. The resolved initial release uses rustls 0.23.43 and `aws-lc-rs` 1.18.0; `Cargo.lock` is the exact version authority.
- Every client and server configuration explicitly selects TLS 1.3 through rustls' protocol-version builder; TLS 1.2 code is not enabled. Cipher suites, key exchange, signature verification, and secure randomness come from the rustls AWS-LC provider. TLS 1.3 is standardized in [RFC 8446](https://www.rfc-editor.org/rfc/rfc8446).
- Route and work admission use HMAC-SHA256 from `aws-lc-rs`, following the standard HMAC construction defined by [RFC 2104](https://www.rfc-editor.org/rfc/rfc2104). Each HMAC key is an independent 32-byte value generated with AWS-LC `SystemRandom`.
- SPKI pins are the lowercase hexadecimal SHA-256 digest of the peer leaf certificate's DER SubjectPublicKeyInfo. The comparison is case-insensitive; each configured value must decode to exactly 32 bytes.
- Certificates are ordinary X.509 certificates validated through rustls/webpki. FlowSplice adds an application identity URI in Subject Alternative Name, whose general PKI form is defined by [RFC 5280](https://www.rfc-editor.org/rfc/rfc5280).
- PEM certificate and private-key loading uses `rustls-pki-types`; the runtime does not depend on the unmaintained `rustls-pemfile` crate.
- UUIDs identify requests, routes, work items, and flows; those identifiers are not secret and possession does not grant admission. The Travel process-session UUID is a deliberate exception: it is generated from the operating system RNG at startup, kept out of logs and deployment files, and acts as a short-lived supplementary capability only after mutual TLS authenticates the same Travel identity.
- FlowSplice does not implement a custom encryption algorithm, custom hash construction, or custom certificate-signature scheme. The `fips` feature is not enabled, so the project makes no FIPS claim.

The disposable E2E certificate script uses the OpenSSL command-line tool to create short-lived P-256 test certificates. OpenSSL is test/build tooling only; it is not linked into or required by the runtime executables. Production certificates are not required to use the E2E script's key type, lifetime, names, or CA keys.

### Three cryptographic layers

| Layer | Endpoints and construction | What it protects |
| --- | --- | --- |
| Management TLS | Home→Server, Server→Relay, and Travel→Relay mutual TLS under the management CA | Peer authentication, catalogs, heartbeats, route requests, and delivery of route/work secrets |
| Route admission | Fixed preface plus HMAC-SHA256 on Travel→Relay and Relay/Home→Server data sockets | Proves possession of a short-lived route or work secret before a socket is paired; it does not encrypt the socket |
| Business TLS | End-to-end mutual TLS from Travel to Home under the separate business CA, tunneled through Relay and Server | Selected service ID, logical TCP/UDP frames, acknowledgements, and business payload plaintext |

The management and business CA roots must be operationally separate. A certificate accepted on the management plane cannot authenticate as a business Travel or Home merely because its role string is similar. Relay and Server do not receive business private keys and are not business TLS endpoints.

The public data path is deliberately nested:

```text
Travel
  └─ HMAC-authenticated Relay route socket
       └─ opaque forwarding through Relay and Server
            └─ end-to-end Travel↔Home mutual business TLS
                 └─ encrypted FlowSplice OPEN/DATA/ACK/FIN/Datagram frames
```

### Certificate identity and authorization checks

Successful TLS chain validation is necessary but not sufficient. FlowSplice applies the following checks in order:

1. rustls/webpki validates the configured trust root, certificate validity, signature chain, and the appropriate client/server extended-key usage. TLS clients also validate the configured server name against the certificate's DNS or IP identity.
2. The application parses the peer leaf certificate and requires exactly one URI beginning `flowsplice://identity/`. The URI must contain a recognized role (`server`, `relay`, `home`, or `travel`) and a non-empty stable ID.
3. The role and ID declared in the first control message must match the certificate-derived identity. Travel also supplies a fresh process-session UUID and a catalog-or-route purpose; neither field can change the certificate-bound identity.
4. Where configured, the SHA-256 SPKI digest must appear in the required allowlist. Empty or malformed required allowlists make the affected application fail at startup.

Current SPKI coverage is directional and explicit:

- Server pins every Relay it connects to.
- Server pins the expected management Home and requires its configured stable Home ID.
- Relay pins its Server and the Travel certificates it accepts.
- Home pins its Server and the business Travel certificates it accepts.
- Travel pins its accepted Relays and the business Home certificate it accepts.

The single active Home session is explicit: a newly authenticated session for the configured Home identity supersedes the previous session, logs the takeover, and actively closes the old session. A different Home ID or a Home key outside Server's allowlist is rejected before registration.

Travel uses a stricter first-wins rule. Each Travel process generates a random session UUID in memory at startup. Its long-lived catalog connection acquires and renews a 45-second Server-held lease through whichever Relay it reaches. Route connections through every Relay may use that same session UUID, preserving concurrent multi-Relay Carrier competition. A different session UUID presenting the same certificate-bound Travel ID is rejected globally by Server, including when it enters through another Relay, and cannot obtain a route. The active session is never displaced by a later login.

An SPKI pin binds the public key rather than the full certificate. Renewing a certificate with the same key preserves the pin; rotating the key requires deploying an overlapping old/new allowlist before switching certificates. CA roots and pins serve different purposes: the CA establishes a valid credential domain, while a pin narrows which keys inside that domain are accepted.

### Route and work secrets

Route allocation intentionally does not reveal the selected service to Relay or Server:

1. An authenticated Travel management connection asks Relay for a route using a random request UUID, its certificate-bound Travel ID, and its already admitted process-session UUID. No service ID is included.
2. Relay forwards the request to Server over management mTLS.
3. Server generates a random 32-byte work secret and work UUID. It sends the same secret to Home and Relay over their authenticated management links and keeps a short-lived in-memory pairing entry.
4. Relay independently generates a random 32-byte route secret and route UUID, returns them to Travel over management mTLS, and keeps a short-lived in-memory route entry.
5. Travel opens the public Relay data socket and sends `FSLCRTE1 || side || route_uuid || HMAC-SHA256(route_secret, FSLCRTE1 || side || route_uuid)`.
6. Relay atomically removes the route entry, verifies the MAC, then connects to Server and authenticates the Relay side with the work secret.
7. Home independently connects to Server and authenticates the Home side with the same work secret.
8. Server accepts at most one Relay side and one Home side, removes the completed work entry, and forwards opaque bytes between them.
9. Travel and Home perform business mTLS through that path. Only after the business handshake succeeds does Travel send the encrypted `OPEN` frame containing the selected service ID.

Route and work entries default to a 15-second lifetime and disappear after use or expiry. Data-preface reads and TLS handshakes have deadlines. The HMAC authenticates the fixed side/UUID preface; it provides neither confidentiality nor a substitute for the inner business TLS. The random secret is never sent on the public data socket—only its HMAC result is.

### Who can see what

| Party | Plaintext and metadata available by design |
| --- | --- |
| Travel | Local mapping plaintext, its configured mappings, catalog, selected service, and business frames before encryption |
| Home | Published catalog, selected service, business frames after decryption, and the final local target connection |
| Server | Home and Relay identities, the Home catalog, Travel ID on a route request, work IDs, connection timing, and forwarded byte volume; no business TLS keys or selected-service `OPEN` plaintext |
| Relay | Server and Travel identities, the distributed catalog, route/work IDs, connection timing, and forwarded byte volume; no business TLS keys or selected-service `OPEN` plaintext |
| Passive network observer | Network endpoints, timing, sizes, route-preface bytes, and visible TLS handshake metadata such as SNI where emitted; not TLS application plaintext |

Relay and Server can drop, delay, duplicate, reorder, or modify forwarded bytes and can deny service. Business TLS detects unauthorized modification but cannot force an untrusted forwarder to provide availability. Relay already receives the complete catalog, so the privacy property is that outer route setup does not reveal which catalog entry Travel selected—not that Relay is unaware of which services exist. Traffic analysis may still correlate a flow with a likely service.

### Secret storage and lifecycle

- Runtime processes load leaf certificates, leaf private keys, and public CA certificates from configured PEM files. CA private keys are not required by any runtime component.
- TLS client/server configurations are built once at process startup and reused. Certificate and key changes therefore take effect after a deliberate process restart rather than being reparsed on every flow.
- Production private keys and configuration files must be protected with filesystem ownership and permissions. The current loader does not provide encrypted-key prompting, an HSM abstraction, or an OS keychain integration.
- Route and work secrets exist only in process memory, are independently generated, are bounded by TTL, and are not persisted across restart. Application logging intentionally records IDs and errors, not secret byte values.
- The Travel process-session UUID is freshly generated at startup, exists only in memory, and is carried only inside authenticated management TLS. It is not packaged with the certificate and does not survive process restart.
- Secret vectors may be cloned while being delivered to the required components and are not backed by a guaranteed zeroizing memory type. Process-memory compromise is outside the current protection boundary.
- There is no automated enrollment, renewal, revocation, CRL, or OCSP workflow. Operators must issue certificates, overlap pins during rotation, remove revoked pins, and restart/reload components through an operational procedure.
- The E2E generator creates unencrypted disposable private keys under the ignored `tests/e2e/generated/` directory. Those keys and CAs must never be reused in production.

### Fail-closed and resource boundaries

- Control and data JSON frames are capped at 1 MiB; a single logical data payload is capped at 64 KiB. The stateful reader preserves partial-frame progress when a `select` branch is cancelled.
- Public TLS handshakes, route-preface reads, control setup frames, route responses, and the business `OPEN` frame default to bounded deadlines. Established control links are disconnected after three missed 10-second heartbeat intervals.
- Pending Server work and Relay routes default to 256 entries each; Home and Travel active-flow limits default to 128.
- Route IDs are unknown after expiry, completed pairings are removed, duplicate sides are rejected, and a consumed Relay route cannot be reused.
- Unexpected message types, identity disagreement, discontinuous TCP offsets, oversized payloads, and invalid route MACs close the affected connection.
- Server atomically admits only the first live process-session UUID for each Travel ID. Catalog leases expire after 45 seconds without renewal; every route request is checked against the active lease before Server allocates work.
- Travel keeps one long-lived catalog subscription. Server pushes catalog changes to Relay and Relay immediately fans them out to connected Travel sessions; this avoids periodic full mTLS polling.
- Server maintains independent control sessions to every configured Relay and publishes the complete Relay directory through each one. Travel needs only one reachable seed to bootstrap the directory.
- TCP Flows retain bounded unacknowledged data independently of a Carrier. Travel detects Carrier failure before Home's longer detach timeout, races replacement Carriers, and Home keeps the target TCP socket while waiting for reattachment.
- UDP association ingress uses bounded non-blocking queues. A saturated association loses its current datagram, consistent with UDP semantics, without blocking unrelated peers.
- Workspace Rust code forbids `unsafe`; this does not mean that every transitive dependency or the AWS-LC C implementation contains no unsafe/native code.

### Current security limits and operator obligations

- Keep Travel UI and TCP/UDP mappings on loopback whenever possible. The UI server is HTTP, not HTTPS. A remotely bound UI requires a bearer token of at least 32 characters, but the application does not measure token entropy and the token is exposed to interception on an untrusted cleartext network unless an external secure tunnel or TLS reverse proxy is used. Generate a high-entropy random token.
- Remotely bound TCP/UDP mappings do not gain bearer-token authentication. `allow_remote_listen = true` only permits the bind; network access must be restricted by a firewall, VPN, or an application-level protocol.
- All business-authorized Travel keys in Home's pin list can request any published service. Per-Travel service ACLs are not implemented.
- Copying a Travel package still copies usable private credentials. First-wins session exclusion prevents a later copy from logging in while the legitimate process keeps renewing its lease, but it does not identify which copy is legitimate. A thief who connects first after lease expiry or Server restart can deny the real device. Prompt pin removal and certificate/key replacement remain the revocation procedure.
- Catalog and Relay-directory integrity are hop-by-hop through management TLS, not end-to-end signed. A compromised Server or Relay can falsify the control data shown to Travel, although Home still refuses an unknown or protocol-mismatched service ID and Travel still requires every contacted Relay to present an allowed certificate identity and key.
- The system does not hide traffic metadata, resist endpoint compromise, guarantee availability against Relay/Server or network denial, provide durable replay state across restart, or claim protection after a CA/private key is stolen.
- The project has not undergone a professional third-party security audit. Deployments should treat this release as an auditable implementation baseline, not as a certified security product.

### Frontend containment

The Travel Agent UI is TypeScript built with Vite, precompressed at build time, and embedded with [embedded-spa v0.1.1](https://github.com/tomcatzh/embedded-spa/tree/v0.1.1) plus `rust-embed`. API routes are mounted before the SPA fallback; missing API routes and hashed assets return real `404` responses. Embedded frontend bytes are public client assets and must never contain private keys, route secrets, bearer tokens, or other confidential configuration.

## Build and check

Requirements: stable Rust, Node.js/npm, CMake, Clang, and Perl. Docker is additionally required for E2E and Linux release artifacts.

```bash
make check
make test
make e2e
make openwrt-ipk
```

`make e2e` generates two temporary test CAs, builds the Linux applications, starts two Relays plus Server, Home, Travel, and TCP/UDP echo targets, and validates:

- TCP and UDP data through the complete topology;
- mutual management TLS and separate Travel-to-Home business TLS;
- single-use HMAC-authenticated route setup;
- complete two-Relay discovery from one configured seed and concurrent full-path Carrier competition;
- global rejection of a second process using the same Travel ID and certificates through either Relay;
- survival of one established client TCP socket and one Home target TCP socket while the selected Relay is killed;
- TLS-1.2 rejection, TLS-1.3 mutual authentication, and incomplete-control-frame expiry;
- the embedded UI, gzip/Brotli selection, representation-specific ETags, and correct `404` boundaries.

The E2E suite enables component `DEBUG` logging, saves the combined log as the ignored `tests/e2e/generated/e2e.log`, and prints its last 300 lines automatically on failure. Production defaults to `INFO`; `RUST_LOG=flowsplice_travelagent=debug,info` (or the corresponding component target) enables per-offset ACK/DUP/retransmission diagnostics. Logs contain operational IDs, offsets, state transitions, and errors—not business payloads, route/work secrets, bearer tokens, or private keys.

Generated keys, E2E logs, web output, build targets, and release binaries are ignored by Git.

`make openwrt-ipk` packages the prebuilt static Linux arm64 Server and Relay binaries with the
generic OpenWrt integration. The package installs one disabled-by-default Server plus any number
of named Relay instances managed by one procd service and one LuCI page. It does not contain
deployment addresses, credentials, firewall policy, or private regression tooling, and it does not
create firewall rules. See [OpenWrt integration](openwrt/README.md).

## Deployment continuity probe

`flowsplice-foobar` provides a loopback-only TCP echo target and a Linux/macOS-friendly CLI probe.
The probe opens one connection, sends one 64-byte sequence record every five seconds, validates the
exact echo, and never reconnects. A timeout, EOF/reset, stale/duplicate data, reordering, or corruption
therefore exits nonzero instead of hiding a broken Flow behind a new connection. See
[foobar/README.md](foobar/README.md) for usage.

## Configuration

Every executable accepts `--config <path>` or `FLOWSPLICE_CONFIG`. Server and Relay also accept
`--check-config` for a side-effect-free configuration and credential validation. Example files live
beside each application:

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

This produces five executables under each of:

- `dist/linux-amd64/` — static PIE, musl;
- `dist/linux-arm64/` — statically linked, musl;
- `dist/macos-arm64/` — self-contained arm64 Mach-O executables.

Linux artifacts are genuinely static. macOS does not support fully static linkage of Apple system libraries; the macOS deliverables are single executables with all FlowSplice code and web assets embedded.

The deterministic IPK builder consumes the Linux arm64 Server and Relay executables rather than
rebuilding them. `make openwrt-ipk` writes an `aarch64_generic` package below `dist/openwrt/`; use
the explicit builder arguments documented in [openwrt/README.md](openwrt/README.md) for another
OpenWrt package architecture or release number.

## Current scope

The current executable supports one active Home Agent, one first-wins live process session per Travel ID, a canonical catalog, a Server-published multi-Relay directory, resilient TCP Flows, and best-effort UDP associations. For each new TCP Flow, Travel concurrently opens complete end-to-end Carriers through all known Relays. Home ACKs the first race arrival and identifies later arrivals as duplicates; Travel keeps the winner and closes the other candidates. It periodically repeats the race, including the active Carrier in that race. The configurable interval doubles up to 15 minutes only when the active Carrier wins again; selecting a different Carrier or completing no race resets the interval to its initial value.

Carrier EOF, reset, TLS/read/write failure, or heartbeat timeout causes immediate recompetition. Travel's recovery timeout is required to be shorter than Home's detach timeout. During that window both endpoints retain bounded unacknowledged data; Home keeps the target TCP connection and retransmits after reattachment. Docker E2E proves this behavior with two Relays and the same endpoint sockets while the active Relay is killed.

This guarantee is in-memory and TCP-specific. Restarting Home destroys its target TCP sockets; restarting Travel destroys its local client sockets. UDP associations currently select the first usable Relay but do not migrate between Carriers. Server can bind multiple explicit IPv4/IPv6 control and data listeners. The OpenWrt package supplies UCI, procd, and LuCI integration for one Server and multiple local Relay processes, including separate LAN and WAN6 Relay identities. Automated enrollment, certificate rotation/revocation, multi-home routing, cross-process session resume, per-Travel service ACLs, and private production deployment remain later work.

## License

[MIT](LICENSE)
