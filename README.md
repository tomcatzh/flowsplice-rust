# FlowSplice

FlowSplice is a small, identity-aware private service access system. It exposes explicitly configured TCP and UDP services from one or more Home Agents to an enrolled travel device without giving the relay or central coordinator access to business plaintext.

This repository is the Rust implementation. It is one Cargo workspace and one Git repository; it does not reuse the previous Go implementation.

> [!WARNING]
> FlowSplice is under active development and is not yet generally available. Non-GA releases may change configuration, protocols, persisted state, and deployment artifacts without backward compatibility.

## How it works

![How FlowSplice works](docs/flowsplice-how-it-works.en.svg)

## Components

| Package | Role |
| --- | --- |
| `flowsplice-server` | Home-side controller, service-catalog and Relay-directory authority, and opaque work-socket coordinator. |
| `flowsplice-relay` | Public management/data ingress and Linux `splice(2)` opaque forwarding. |
| `flowsplice-homeagent` | Publishes configured services, terminates business TLS, connects flows to home targets, and serves the password-gated Travel issuer/revocation UI. |
| `flowsplice-travelagent` | Creates local TCP/UDP mappings, originates business TLS, and serves the embedded TypeScript UI. |
| `flowsplice-foobar` | Low-rate single-TCP-connection loopback target and CLI continuity probe for deployment acceptance. |
| `flowsplice-core` | Shared protocol framing, route-ticket authentication, TLS identity, and configuration support. |

The management plane uses mutual TLS. Every leaf certificate contains exactly one FlowSplice URI SAN in the form `flowsplice://identity/<role>/<id>`. Management and business traffic use separate CA roots. Server, Relay, and Home relationships are narrowed with SHA-256 SPKI allowlists; each Travel installation instead requires at least one authority-signed grant binding its management/business SPKIs, scope, and validity to a revocable credential ID. Business TLS is terminated only by Travel and Home; Relay and Server forward its bytes without possessing the business private keys. Exact trust, visibility, and current limitations are documented below.

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
- keep business plaintext and the selected service ID unavailable to Relay, Server, and passive network observers; the selected Home ID is route metadata;
- prevent an unauthenticated socket from joining an allocated route merely by guessing an ID;
- constrain a management-key compromise from becoming a business endpoint credential;
- fail closed on malformed identity, certificate, pin, route-authentication, and bounded-frame checks.

FlowSplice does not attempt to hide IP addresses, connection timing, byte counts, TLS handshake metadata, the existence of a Home or Travel identity, or the service catalog from the control-plane components that distribute it. It also does not protect plaintext after Travel, Home, or the final local service endpoint is compromised.

### Cryptographic primitives and versions

- TLS is implemented by [`rustls`](https://docs.rs/rustls/latest/rustls/) through `tokio-rustls`. Cargo disables rustls default features and explicitly enables only the `aws_lc_rs` and `std` features.
- Every executable calls `rustls::crypto::aws_lc_rs::default_provider().install_default()` during startup. The resolved initial release uses rustls 0.23.43 and `aws-lc-rs` 1.18.0; `Cargo.lock` is the exact version authority.
- Every client and server configuration explicitly selects TLS 1.3 through rustls' protocol-version builder; TLS 1.2 code is not enabled. Cipher suites, key exchange, signature verification, and secure randomness come from the rustls AWS-LC provider. TLS 1.3 is standardized in [RFC 8446](https://www.rfc-editor.org/rfc/rfc8446).
- Route and work admission use HMAC-SHA256 from `aws-lc-rs`, following the standard HMAC construction defined by [RFC 2104](https://www.rfc-editor.org/rfc/rfc2104). Each HMAC key is an independent 32-byte value generated with AWS-LC `SystemRandom`.
- Travel credentials use detached ECDSA P-256/SHA-256 signatures in ASN.1 DER form. Each trusted Home authority has its own signing key; an optional, separately configured global authority is a higher-privilege capability. Server and Relay receive only the uncompressed P-256 public keys.
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
4. Non-Travel peers must appear in the required SHA-256 SPKI allowlist. A Travel peer must match the stable ID and management/business SPKIs in at least one trusted, signed credential grant that is neither revoked nor outside its validity interval. The selected grant must authorize the exact Home or logical service being opened.

Current SPKI coverage is directional and explicit:

- Server pins every Relay it connects to.
- Server has an explicit allowlist of Home IDs and pins the management key set for each Home independently.
- Relay pins its Server and resolves Travel management certificates through the synchronized signed-credential set.
- Home pins its Server and resolves Travel business certificates through the same credential ID.
- Travel pins its accepted Relays and independently pins each configured Home ID, TLS server name, and business certificate key set.

Home sessions are isolated by stable Home ID. A newly authenticated session supersedes and closes only the previous session for that same Home ID. Other configured Homes remain online; an unknown Home ID or a key outside that Home's allowlist is rejected before registration.

Travel uses a stricter first-wins rule. Each Travel process generates a random session UUID in memory at startup. Its long-lived catalog connection acquires and renews a 45-second Server-held lease for the stable Travel ID through whichever Relay it reaches. Route connections through every Relay may use that same session UUID, preserving concurrent multi-Relay Carrier competition. A different session UUID presenting the same Travel TLS identity is rejected globally by Server, including when it enters through another Relay, and cannot obtain a route. Multiple credential grants for that identity do not create multiple login slots. The active session is never displaced by a later login.

An SPKI pin binds the public key rather than the full certificate. For non-Travel peers, renewing a certificate with the same key preserves the pin; rotating the key requires an overlapping allowlist. Each signed Travel grant binds two SPKIs, one stable Travel ID, one random credential ID, one explicit scope, and one validity interval. One TLS identity may hold several grants, but management and business certificates cannot be mixed across credential IDs.

### Travel credential issuance and live revocation

Each Travel installation generates two distinct P-256 keys locally and stores them as password-encrypted PKCS#8 files. `enroll-init` emits only a public enrollment request containing two proof-of-possession CSRs; no Travel private key leaves that device.

Issuance belongs to Home, not Server. The Home Agent serves a separate loopback-only embedded SPA (default `127.0.0.1:9081`). An operator uploads the request, chooses a Global, Home, or exact `(home_id, service_id, protocol)` scope, selects a validity period (365 days by default), and enters the signing-key password. Home decrypts the management CA, business CA, and selected authority key only for that request, signs both certificates and the scoped authorization grant, returns the enrollment response, and publishes only the signed public grant to Server. The password is zeroized after use and is never stored by FlowSplice.

A normal Home authority can sign only grants for its own Home and services. The optional global authority is a separate super-authority configured only on a designated Home; possessing a normal Home key cannot mint global access. Server authenticates the publishing Home against the configured authority owner, verifies the signature and scope, persists the add-only credential set, increments the authorization generation, and broadcasts it to every Relay and Home. Server and OpenWrt never hold the CA or authority private keys.

```bash
flowsplice-travelagent enroll-init \
  --travel-id travel-laptop \
  --output-dir ./travel-laptop

# Open the Home issuer UI, upload:
#   ./travel-laptop/enrollment-request.json
# Choose the scope and validity, enter the issuer-key password, then download the response.

flowsplice-travelagent enroll-import \
  --enrollment-dir ./travel-laptop \
  --response ./travel-laptop-response.json \
  --management-ca ./management-ca.crt \
  --business-ca ./business-ca.crt
```

Travel verifies the response against its original request, encrypted local keys, both CA roots, and the authority public key carried by and cryptographically bound to the response. A corrupted, mismatched, expired, or wrong-password import fails closed.

Revocation is initiated by the Home issuer UI/API. Server accepts it only from the authenticated Home that owns the credential's signing authority, durably appends the credential ID and reason, then publishes the new monotonic generation over existing control sessions. Every Relay and Home verifies signatures, rejects generation rollback or loss of a previously observed revocation, persists anti-rollback state, atomically applies the update, and acknowledges the generation. A revoked or expired credential cannot open new sessions or Carriers; state owned by that credential is closed without restarting Server, Relay, or Home. Revocation is irreversible, so replacement uses fresh keys, certificates, and a new credential ID.

### Route and work secrets

Each Travel mapping identifies one logical business by `(home_id, service_id, protocol)` and binds it to one local listener. Route allocation reveals the selected Home ID so Server can choose the correct Home session, but it does not reveal the selected service ID to Relay or Server:

1. An authenticated Travel management connection asks Relay for a route using a random request UUID, its certificate-bound Travel ID, its already admitted process-session UUID, and the mapping's Home ID. No service ID is included.
2. Relay selects an active signed grant for that Travel identity which authorizes the requested Home, then forwards the request and selected credential ID to Server over management mTLS.
3. Server selects exactly the requested configured Home, generates a random 32-byte work secret and work UUID, sends the same secret to that Home and Relay over their authenticated management links, and keeps a short-lived in-memory pairing entry.
4. Relay independently generates a random 32-byte route secret and route UUID, returns them to Travel over management mTLS, and keeps a short-lived in-memory route entry.
5. Travel opens the public Relay data socket and sends `FSLCRTE1 || side || route_uuid || HMAC-SHA256(route_secret, FSLCRTE1 || side || route_uuid)`.
6. Relay atomically removes the route entry, verifies the MAC, then connects to Server and authenticates the Relay side with the work secret.
7. Home independently connects to Server and authenticates the Home side with the same work secret.
8. Server accepts at most one Relay side and one socket holding the selected Home's secret, removes the completed work entry, and forwards opaque bytes between them.
9. Travel and Home perform business mTLS through that path. Only after the business handshake succeeds does Travel send the encrypted `OPEN` frame containing the selected service ID.

Route and work entries default to a 15-second lifetime and disappear after use or expiry. Data-preface reads and TLS handshakes have deadlines. The HMAC authenticates the fixed side/UUID preface; it provides neither confidentiality nor a substitute for the inner business TLS. The random secret is never sent on the public data socket—only its HMAC result is.

### Who can see what

| Party | Plaintext and metadata available by design |
| --- | --- |
| Travel | Local mapping plaintext, its configured mappings, catalog, selected service, and business frames before encryption |
| Home | Published catalog, selected service, business frames after decryption, and the final local target connection |
| Server | Home and Relay identities, all Home catalogs, Travel ID and selected Home ID on a route request, work IDs, connection timing, and forwarded byte volume; no business TLS keys or selected-service `OPEN` plaintext |
| Relay | Server and Travel identities, the distributed multi-Home catalog, selected Home ID, route/work IDs, connection timing, and forwarded byte volume; no business TLS keys or selected-service `OPEN` plaintext |
| Passive network observer | Network endpoints, timing, sizes, route-preface bytes, and visible TLS handshake metadata such as SNI where emitted; not TLS application plaintext |

Relay and Server can drop, delay, duplicate, reorder, or modify forwarded bytes and can deny service. Business TLS detects unauthorized modification but cannot force an untrusted forwarder to provide availability. Relay already receives the complete catalog. Outer route setup reveals which Home is selected but not which service entry within that Home; traffic analysis may still correlate a flow with a likely service.

### Secret storage and lifecycle

- Runtime processes load leaf certificates, leaf private keys, and public CA certificates from configured PEM files. Travel requires both of its private keys to be password-encrypted; other ordinary runtime identities currently rely on filesystem-protected keys. A Home configured for issuance additionally holds password-encrypted management/business CA keys and one or more password-encrypted authority keys.
- TLS client/server configurations are built once at process startup and reused. Certificate and key changes therefore take effect after a deliberate process restart rather than being reparsed on every flow.
- Production private keys and configuration files must be protected with filesystem ownership and permissions. Travel prompts for its private-key password at enrollment, import, and process start. Home requires the issuer-key password for each signing request and does not persist it. HSM-backed signing and automatic keychain integration are not implemented.
- Route and work secrets exist only in process memory, are independently generated, are bounded by TTL, and are not persisted across restart. Application logging intentionally records IDs and errors, not secret byte values.
- The Travel process-session UUID is freshly generated at startup, exists only in memory, and is carried only inside authenticated management TLS. It is not packaged with the certificate and does not survive process restart.
- Travel-authorization signing keys exist only on the Home instances explicitly configured to issue. Server persists the signed credential set and monotonic revocation generation; Relay and Home persist the highest generation and observed revoked credential IDs so rollback remains fail-closed across restart.
- Secret vectors may be cloned while being delivered to the required components and are not backed by a guaranteed zeroizing memory type. Process-memory compromise is outside the current protection boundary.
- Automatic unattended signing, key renewal, CRL, and OCSP are not implemented. Local key generation, CSR proof of possession, password-gated Home dual-CA issuance, scoped authority signatures, signed-response import, and live application-layer credential revocation are implemented; key replacement creates a new signed credential ID because revocation cannot be undone.
- The E2E generator creates disposable identities under the ignored `tests/e2e/generated/` directory. Its disposable CA/authority keys and Travel keys are encrypted with a test-only password-file path; non-Travel disposable leaf keys remain unencrypted. None of this material may be reused in production.

### Fail-closed and resource boundaries

- Control and data JSON frames are capped at 1 MiB; a single logical data payload is capped at 64 KiB. The stateful reader preserves partial-frame progress when a `select` branch is cancelled.
- Public TLS handshakes, route-preface reads, control setup frames, route responses, and the business `OPEN` frame default to bounded deadlines. Established control links are disconnected after three missed 10-second heartbeat intervals.
- Pending Server work and Relay routes default to 256 entries each; Home and Travel active-flow limits default to 128.
- Route IDs are unknown after expiry, completed pairings are removed, duplicate sides are rejected, and a consumed Relay route cannot be reused.
- Unexpected message types, identity disagreement, discontinuous TCP offsets, oversized payloads, and invalid route MACs close the affected connection.
- Server atomically admits only the first live process-session UUID for each stable Travel ID. Catalog leases expire after 45 seconds without renewal; every route request is checked against that lease and a currently active grant for the requested Home before Server allocates work.
- Travel keeps one long-lived catalog subscription. Server pushes catalog changes to Relay and Relay immediately fans them out to connected Travel sessions; this avoids periodic full mTLS polling.
- Server maintains independent control sessions to every configured Relay and publishes the complete Relay directory through each one. Travel needs only one reachable seed to bootstrap the directory.
- TCP Flows retain bounded unacknowledged data independently of a Carrier. Travel detects Carrier failure before Home's longer detach timeout, races replacement Carriers, and Home keeps the target TCP socket while waiting for reattachment.
- UDP association ingress uses bounded non-blocking queues. A saturated association loses its current datagram, consistent with UDP semantics, without blocking unrelated peers.
- Workspace Rust code forbids `unsafe`; this does not mean that every transitive dependency or the AWS-LC C implementation contains no unsafe/native code.

### Current security limits and operator obligations

- Keep Travel UI and TCP/UDP mappings on loopback whenever possible. The UI server is HTTP, not HTTPS. A remotely bound UI requires a bearer token of at least 32 characters, but the application does not measure token entropy and the token is exposed to interception on an untrusted cleartext network unless an external secure tunnel or TLS reverse proxy is used. Generate a high-entropy random token.
- Remotely bound TCP/UDP mappings do not gain bearer-token authentication. `allow_remote_listen = true` only permits the bind; network access must be restricted by a firewall, VPN, or an application-level protocol.
- A Travel identity may hold several independently revocable grants. Global grants authorize every Home, Home grants authorize one Home, and Service grants authorize one exact `(home_id, service_id, protocol)` business. Relay selects a matching active grant for each route; grants never widen themselves or fall back to another Home.
- Copying a Travel package still copies usable private credentials. First-wins session exclusion prevents a later copy from logging in while the legitimate process keeps renewing its lease, but it does not identify which copy is legitimate. A stolen Travel identity must have every active grant revoked; restoring the legitimate device requires new keys, certificates, and newly signed grants.
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

`make e2e` generates two temporary test CAs, builds the Linux applications, starts two Relays, two Home Agents, Server, Travel, and TCP/UDP echo targets, and validates:

- password-encrypted Travel-local key generation, CSR proof of possession, password-gated Home dual-CA signing, Global/Home/Service scopes, response import, default one-year validity, and exact 30-minute test validity;
- TCP and UDP data through the complete topology;
- mutual management TLS and separate Travel-to-Home business TLS;
- single-use HMAC-authenticated route setup;
- complete two-Relay discovery from one configured seed and concurrent full-path Carrier competition;
- a two-Home catalog, exact `(Home, service)` routing when both Homes publish the same service ID, no cross-Home fallback, isolated Home removal/rejoin, and continued availability of the other Home;
- global rejection of a second process using the same Travel ID and certificates through either Relay;
- survival of one established client TCP socket and one Home target TCP socket while the selected Relay is killed;
- TLS-1.2 rejection, TLS-1.3 mutual authentication, and incomplete-control-frame expiry;
- live add-only credential import and revocation, all-Relay/all-Home generation acknowledgements, existing-Flow termination on both Homes, both-Relay rejection, idempotency, and revocation persistence after Relay restart without authorization-induced process restarts;
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

## Tutorial: enroll a new Travel device

This is the ordinary attended enrollment path. The Travel device creates and keeps its own private
keys; a Home Agent approves the requested access and publishes only the signed public grant through
Server. Do not copy an existing Travel directory to a new device—create a new stable Travel ID and
new keys instead.

### 1. Generate the request on the Travel device

```bash
flowsplice-travelagent enroll-init \
  --travel-id travel-laptop \
  --output-dir ./travel-laptop
```

Enter a new password of at least 12 characters twice. This password encrypts the Travel device's
management and business private keys and is required again when importing the response and starting
Travel Agent.

The directory now contains:

- `travel-management.key` and `travel-business.key`: encrypted private keys; never upload or share
  them;
- `enrollment-state.json`: private local state used to bind the response to this exact request;
- `enrollment-request.json`: the only file to send to the Home operator.

### 2. Sign it on Home

On the Home machine, open the Home Agent issuer page:

```text
http://127.0.0.1:9081
```

`9081` is the example and current deployment port; the authoritative value is `[issuer].listen` in
the Home configuration. The listener is loopback-only by default. When administering another Home,
use a trusted local tunnel rather than exposing this page directly to WAN.

Upload `enrollment-request.json`, select the validity period, and choose the narrowest suitable
scope:

- **Specified service** authorizes one exact `(home_id, service_id, protocol)` business;
- **Current Home** authorizes all current and future services published by that Home;
- **Global super authorization** authorizes every Home and appears only on a designated Home that
  holds the separately configured global authority.

Enter the **Home issuer-key password** and select **签发并下载结果 (Issue and download)**. This is the password that
decrypts the Home's CA and authority keys; it is not the Travel private-key password and it is not a
public key. FlowSplice uses it for that signing operation and does not store it.

The browser downloads `flowsplice-travel-laptop-response.json`. Home also publishes the signed
public grant to Server automatically, so Server, Relay, and the other Homes learn it without a
restart. No Travel private key or issuer private key is uploaded to Server.

### 3. Import the response on the same Travel device

Copy the downloaded response plus the public management and business CA certificates to the Travel
device, then run:

```bash
flowsplice-travelagent enroll-import \
  --enrollment-dir ./travel-laptop \
  --response ./flowsplice-travel-laptop-response.json \
  --management-ca ./management-ca.crt \
  --business-ca ./business-ca.crt
```

Enter the Travel private-key password created in step 1. Import verifies the original request and
local keys, both certificate chains, validity, scope, authority signature, and stable Travel ID. On
success it adds `travel-management.crt`, `travel-business.crt`, and the verified
`enrollment-response.json` to the enrollment directory. A response created for another request or
key pair fails closed.

### 4. Configure and start Travel Agent

Start from [travelagent/config.example.toml](travelagent/config.example.toml). At minimum:

- set `id` to the same stable ID passed to `enroll-init`;
- point the management/business certificate and key paths at the imported enrollment directory;
- install the two public CA certificates;
- configure at least one reachable seed Relay plus the accepted Relay SPKI pins;
- configure every permitted Home ID, TLS server name, and Home business SPKI pin;
- add a loopback mapping for the intended `(home_id, service_id, protocol)`.

Then start the process:

```bash
flowsplice-travelagent --config ./travelagent.toml
```

Enter the Travel private-key password when prompted. Travel learns the complete authorized Relay
directory after reaching any seed and races the reachable Relays for each new TCP Flow. Keep local
mappings on `127.0.0.1` unless remote exposure is an explicit, separately protected requirement.

For a Foobar mapping on `127.0.0.1:10080`, a bounded end-to-end check is:

```bash
flowsplice-foobar probe --addr 127.0.0.1:10080 --count 5
```

It sends one exact record every five seconds over one TCP connection and fails rather than silently
reconnecting.

### 5. Revoke access

Return to the same Home issuer page, find the credential, and select **撤销 (Revoke)**. Revocation is
irreversible and is distributed through Server without restarting Server, Relay, or Home. A stolen
Travel identity may have several grants; revoke every active grant bound to that Travel ID, then
generate fresh keys and a new enrollment instead of reusing the compromised directory.

## Configuration

Every executable accepts `--config <path>` or `FLOWSPLICE_CONFIG`. Server and Relay also accept
`--check-config` for a side-effect-free configuration and credential validation. Example files live
beside each application:

- [server/config.example.toml](server/config.example.toml)
- [relay/config.example.toml](relay/config.example.toml)
- [homeagent/config.example.toml](homeagent/config.example.toml)
- [travelagent/config.example.toml](travelagent/config.example.toml)

The E2E certificate generator is disposable test tooling only. Production Travel identities use `flowsplice-travelagent enroll-init` and `enroll-import`; issuance and revocation are performed through the selected Home Agent's separate local UI/API. Operators must provision and protect the Home issuer's encrypted management/business CA keys, Home authority key, optional global authority key, non-Travel leaf keys, renewal process, and SPKI allowlists. Server, Relay, and OpenWrt configs contain only the trusted authority records and public keys. Startup fails when required trust or authorization state is missing or malformed.

The Travel UI and local mappings bind to loopback by default. A non-loopback UI requires `allow_remote_listen = true` and an administrator bearer token of at least 32 characters.

## Release artifacts

```bash
./scripts/build-release.sh
```

This produces five executables (`flowsplice-server`, `flowsplice-relay`, `flowsplice-homeagent`, `flowsplice-travelagent`, and `flowsplice-foobar`) under each of:

- `dist/linux-amd64/` — static PIE, musl;
- `dist/linux-arm64/` — statically linked, musl;
- `dist/macos-arm64/` — self-contained arm64 Mach-O executables.

Linux artifacts are genuinely static. macOS does not support fully static linkage of Apple system libraries; the macOS deliverables are single executables with all FlowSplice code and web assets embedded.

The deterministic IPK builder consumes the Linux arm64 Server and Relay executables rather than
rebuilding them. `make openwrt-ipk` writes an `aarch64_generic` package below `dist/openwrt/`; use
the explicit builder arguments documented in [openwrt/README.md](openwrt/README.md) for another
OpenWrt package architecture or release number.

## Current scope

The current executable supports multiple simultaneously active Home Agents, one first-wins live process session per stable Travel identity, multiple scoped authority grants per Travel, password-gated Home issuance, live Home-originated revocation, an aggregated multi-Home catalog, a Server-published multi-Relay directory, resilient TCP Flows, and best-effort UDP associations. A logical business is the explicit `(home_id, service_id, protocol)` selected by a Travel mapping; a Flow never falls back to another Home. For each new TCP Flow, Travel concurrently opens complete end-to-end Carriers through all known Relays to that one Home. The selected Home ACKs the first race arrival and identifies later arrivals as duplicates; Travel keeps the winner and closes the other candidates. It periodically repeats the race, including the active Carrier in that race. The configurable interval doubles up to 15 minutes only when the active Carrier wins again; selecting a different Carrier or completing no race resets the interval to its initial value.

Carrier EOF, reset, TLS/read/write failure, or heartbeat timeout causes immediate recompetition. Travel's recovery timeout is required to be shorter than Home's detach timeout. During that window both endpoints retain bounded unacknowledged data; Home keeps the target TCP connection and retransmits after reattachment. Docker E2E proves this behavior with two Relays and the same endpoint sockets while the active Relay is killed.

This guarantee is in-memory and TCP-specific. Restarting the selected Home destroys its target TCP sockets; restarting Travel destroys its local client sockets. Other Home Agents are independent but are not failover substitutes for that logical business. UDP associations currently select the first usable Relay but do not migrate between Carriers. Server has one Home control listener shared by all configured Home identities and may bind multiple explicit IPv4/IPv6 data listeners. The OpenWrt package supplies UCI, procd, bilingual LuCI integration, trusted Home/authority lists, and one Server plus multiple local Relay processes, including separate LAN and WAN6 Relay identities. Unattended signing, certificate rotation, cross-process Flow resume, and GA compatibility guarantees remain later work.

## License

[MIT](LICENSE)
