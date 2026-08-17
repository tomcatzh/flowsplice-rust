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

The management plane uses mutual TLS. Every leaf certificate contains exactly one FlowSplice URI SAN in the form `flowsplice://identity/<role>/<id>`. Management and business traffic use separate CA roots, and selected peer relationships are narrowed further with SHA-256 SPKI allowlists. Business TLS is terminated only by Travel and Home; Relay and Server forward its bytes without possessing the business private keys. Exact trust, visibility, and current limitations are documented below.

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

- TLS is implemented by [`rustls`](https://docs.rs/rustls/latest/rustls/) through `tokio-rustls`. Cargo disables rustls default features and explicitly enables the `aws_lc_rs`, `std`, and `tls12` features.
- Every executable calls `rustls::crypto::aws_lc_rs::default_provider().install_default()` during startup. The resolved initial release uses rustls 0.23.43 and `aws-lc-rs` 1.18.0; `Cargo.lock` is the exact version authority.
- The current builders use rustls' default protocol versions and cipher-suite selection. Because the `tls12` feature is enabled, the current implementation permits TLS 1.2 and TLS 1.3; it does not enforce TLS-1.3-only operation. TLS 1.3 is standardized in [RFC 8446](https://www.rfc-editor.org/rfc/rfc8446).
- Route and work admission use HMAC-SHA256 from `aws-lc-rs`, following the standard HMAC construction defined by [RFC 2104](https://www.rfc-editor.org/rfc/rfc2104). Each HMAC key is an independent 32-byte value generated with AWS-LC `SystemRandom`.
- SPKI pins are the lowercase hexadecimal SHA-256 digest of the peer leaf certificate's DER SubjectPublicKeyInfo. The comparison is case-insensitive; each configured value must decode to exactly 32 bytes.
- Certificates are ordinary X.509 certificates validated through rustls/webpki. FlowSplice adds an application identity URI in Subject Alternative Name, whose general PKI form is defined by [RFC 5280](https://www.rfc-editor.org/rfc/rfc5280).
- UUIDs identify requests, routes, work items, and flows, but UUID secrecy is not a security boundary. Possession of the corresponding random HMAC secret or a valid mutually authenticated TLS identity is what grants admission.
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
3. The role and ID declared in the first `HELLO` control message must match the certificate-derived identity. A JSON field cannot promote a certificate into a different role.
4. Where configured, the SHA-256 SPKI digest must appear in the required allowlist. Empty or malformed required allowlists make the affected application fail at startup.

Current SPKI coverage is directional and explicit:

- Server pins the Relay it connects to.
- Relay pins its Server and the Travel certificates it accepts.
- Home pins its Server and the business Travel certificates it accepts.
- Travel pins its Relay and the business Home certificate it accepts.
- Server currently does **not** have a Home SPKI allowlist or configured expected Home ID. It accepts a Home management certificate signed by the management CA when its URI role is `home` and its `HELLO` ID matches the certificate. Therefore the management CA is the current Server-side Home enrollment authority.

An SPKI pin binds the public key rather than the full certificate. Renewing a certificate with the same key preserves the pin; rotating the key requires deploying an overlapping old/new allowlist before switching certificates. CA roots and pins serve different purposes: the CA establishes a valid credential domain, while a pin narrows which keys inside that domain are accepted.

### Route and work secrets

Route allocation intentionally does not reveal the selected service to Relay or Server:

1. An authenticated Travel management connection asks Relay for a route using a random request UUID and its certificate-bound Travel ID. No service ID is included.
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
- Production private keys and configuration files must be protected with filesystem ownership and permissions. The current loader does not provide encrypted-key prompting, an HSM abstraction, or an OS keychain integration.
- Route and work secrets exist only in process memory, are independently generated, are bounded by TTL, and are not persisted across restart. Application logging intentionally records IDs and errors, not secret byte values.
- Secret vectors may be cloned while being delivered to the required components and are not backed by a guaranteed zeroizing memory type. Process-memory compromise is outside the current protection boundary.
- There is no automated enrollment, renewal, revocation, CRL, or OCSP workflow. Operators must issue certificates, overlap pins during rotation, remove revoked pins, and restart/reload components through an operational procedure.
- The E2E generator creates unencrypted disposable private keys under the ignored `tests/e2e/generated/` directory. Those keys and CAs must never be reused in production.

### Fail-closed and resource boundaries

- Control and data JSON frames are capped at 1 MiB; a single logical data payload is capped at 64 KiB.
- Public TLS handshakes and route-preface reads default to 10-second deadlines.
- Pending Server work and Relay routes default to 256 entries each; Home and Travel active-flow limits default to 128.
- Route IDs are unknown after expiry, completed pairings are removed, duplicate sides are rejected, and a consumed Relay route cannot be reused.
- Unexpected message types, identity disagreement, discontinuous TCP offsets, oversized payloads, and invalid route MACs close the affected connection.
- Workspace Rust code forbids `unsafe`; this does not mean that every transitive dependency or the AWS-LC C implementation contains no unsafe/native code.

### Current security limits and operator obligations

- Keep Travel UI and TCP/UDP mappings on loopback whenever possible. The UI server is HTTP, not HTTPS. A remotely bound UI requires a bearer token of at least 32 characters, but the application does not measure token entropy and the token is exposed to interception on an untrusted cleartext network unless an external secure tunnel or TLS reverse proxy is used. Generate a high-entropy random token.
- Remotely bound TCP/UDP mappings do not gain bearer-token authentication. `allow_remote_listen = true` only permits the bind; network access must be restricted by a firewall, VPN, or an application-level protocol.
- All business-authorized Travel keys in Home's pin list can request any published service. Per-Travel service ACLs are not implemented.
- Catalog integrity is hop-by-hop through management TLS, not end-to-end signed by Home. A compromised Server or Relay can falsify the catalog shown to Travel, although Home still refuses an unknown or protocol-mismatched service ID.
- TLS 1.2 remains enabled. Enforcing TLS 1.3 only requires an implementation and compatibility decision, not merely a configuration assumption.
- Server-side Home trust currently rests on the management CA plus certificate/HELLO role-ID consistency rather than a Home SPKI pin.
- The system does not hide traffic metadata, resist endpoint compromise, guarantee availability against Relay/Server or network denial, provide durable replay state across restart, or claim protection after a CA/private key is stolen.
- The project has not undergone an independent security audit. Deployments should treat this release as an auditable implementation baseline, not as a certified security product.

### Frontend containment

The Travel Agent UI is TypeScript built with Vite, precompressed at build time, and embedded with [embedded-spa v0.1.1](https://github.com/tomcatzh/embedded-spa/tree/v0.1.1) plus `rust-embed`. API routes are mounted before the SPA fallback; missing API routes and hashed assets return real `404` responses. Embedded frontend bytes are public client assets and must never contain private keys, route secrets, bearer tokens, or other confidential configuration.

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
