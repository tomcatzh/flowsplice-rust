# FlowSplice

FlowSplice is a small, identity-aware private service access system. It exposes explicitly configured TCP and UDP services from one or more Home Agents to an enrolled travel device without giving the relay or central coordinator access to business plaintext.

This repository is the Rust implementation. It is one Cargo workspace and one Git repository; it does not reuse the previous Go implementation.

> [!WARNING]
> FlowSplice is under active development and is not yet generally available. Non-GA releases may change configuration, protocols, persisted state, and deployment artifacts without backward compatibility.

## How it works

![How FlowSplice works](docs/flowsplice-how-it-works.en.svg)

[中文图示](docs/flowsplice-how-it-works.svg)

## Components

| Component | Role |
| --- | --- |
| `flowsplice-server` | Home-side controller, service-catalog and Relay-directory authority, and opaque work-socket coordinator. |
| `flowsplice-relay` | Public management/data ingress and Linux `splice(2)` opaque forwarding. |
| `flowsplice-homeagent` | Publishes configured services, terminates business TLS, connects flows to home targets, and serves the password-gated Travel issuer/revocation/key-maintenance UI. |
| `flowsplice-travelagent` | Creates local TCP/UDP mappings, originates business TLS, and serves the embedded TypeScript UI. |
| `flowsplice-foobar` | Low-rate single-TCP-connection loopback target and CLI continuity probe for deployment acceptance. |
| `flowsplice-core` | Shared protocol framing, route-ticket authentication, TLS identity, and configuration support. |
| `flowsplice-enrollment` | Device-local Travel enrollment, certificate issuance, signed grants, and import validation. |

The management plane uses mutual TLS. Every leaf certificate contains exactly one FlowSplice URI SAN in the form `flowsplice://identity/<role>/<id>`. Management and business traffic use separate CA roots. Server, Relay, and Home relationships are narrowed with SHA-256 SPKI allowlists; each Travel installation instead requires at least one authority-signed grant binding its management/business SPKIs, scope, and validity to a revocable credential ID. Business TLS is terminated only by Travel and Home; Relay and Server forward its bytes without possessing the business private keys. Exact trust, visibility, and current limitations are documented below.

See [Architecture](docs/architecture.md) for the detailed boundary and protocol flow.
See [Security Audit Remediation — 2026-08-17](docs/security-audit-remediation-2026-08-17.md) for the finding-by-finding verification and disposition of the independent Kimi K3 review.

## Repository layout

```text
crates/                   shared core and enrollment crates
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
- Every executable installs the rustls AWS-LC provider during startup. The current lockfile resolves rustls 0.23.43 and `aws-lc-rs` 1.18.0; `Cargo.lock` is the exact version authority.
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

Issuance belongs to Home, not Server. The Home Agent serves a separate loopback-only embedded SPA (the example configuration uses `127.0.0.1:9081`). An operator uploads the request, chooses a Global, Home, or exact `(home_id, service_id, protocol)` scope, selects a validity period (365 days by default), and enters the signing-key password. Home decrypts the management CA, business CA, and selected authority key only for that request, signs both certificates and the scoped authorization grant, returns the enrollment response, and publishes only the signed public grant to Server. The backend wraps the received password in zeroizing storage for the signing operation and does not persist it.

A normal Home authority can sign only grants for its own Home and services. The optional global authority is a separate super-authority configured only on a designated Home; possessing a normal Home key cannot mint global access. Server authenticates the publishing Home against the configured authority owner, verifies the signature and scope, persists the add-only credential set, increments the authorization generation, and broadcasts it to every Relay and Home. Server and OpenWrt never hold the CA or authority private keys.

Travel verifies the response against its original request, encrypted local keys, both CA roots, and the authority signature. The response-carried authority key provides an integrity check during import; runtime authorization independently requires that authority to be configured as trusted on Server, Relay, and Home. A corrupted, mismatched, expired, or wrong-password import fails closed. See [Enroll a new Travel device](#tutorial-enroll-a-new-travel-device) for the complete operator workflow.

Revocation is initiated by the Home issuer UI/API. Server accepts it only from the authenticated Home that owns the credential's signing authority, durably appends the credential ID and reason, then publishes the new monotonic generation over existing control sessions. Relay and Home verify the update, reject rollback or loss of an observed revocation, persist anti-rollback state, and acknowledge the generation. A revoked or expired credential cannot open a new session or Carrier. Authorization-side state closes without restarting any component; an already-open Travel-side local TCP socket may remain in its normal Carrier-recovery loop until that shorter recovery deadline expires, but no revoked Carrier can reattach. Revocation is irreversible, so replacement uses fresh keys and a new credential ID.

### Route and work secrets

Each Travel mapping identifies one logical business by `(home_id, service_id, protocol)` and binds it to one local listener. Route allocation reveals the selected Home ID so Server can choose the correct Home session, but it does not reveal the selected service ID to Relay or Server:

1. Authenticated Travel asks a Relay for the selected Home. Relay chooses an active grant that covers that Home and asks Server to allocate work for exactly that Home session.
2. Server creates a random 32-byte work secret for Relay/Home pairing; Relay independently creates a random 32-byte route secret for Travel/Relay admission. Both are delivered only over management mTLS.
3. Travel authenticates the public data socket with `HMAC-SHA256(route_secret, fixed-preface)`. Relay and Home separately prove possession of the work secret to Server. Each route is short-lived and single-use.
4. Travel and Home then perform business mTLS through the paired opaque path. Only the encrypted `OPEN` frame reveals the service ID to Home.

Route and work entries default to a 15-second lifetime and disappear after use or expiry. HMAC authenticates admission only; confidentiality and endpoint identity come from the inner business TLS. See [Architecture](docs/architecture.md) for the complete message sequence.

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

- Travel requires password-encrypted management and business keys. A Home issuer additionally holds password-encrypted management/business CA keys and Home authority keys; the optional global authority is a separate higher-privilege key. Other runtime leaf keys currently rely on filesystem protection.
- TLS identities are loaded at startup, so certificate or leaf-key rotation requires a deliberate process restart. Signed Travel grants and revocations update live without restarting Server, Relay, or Home.
- Travel prompts for its key password during enrollment, import, and startup. Home requires its issuer password for each signing request and does not persist it. The loopback-only Home and Travel UIs can re-encrypt all keys in their respective password groups after verifying the current password; neither component stores passwords or integrates with the system keychain. HSM-backed signing, renewal, CRL, and OCSP are not implemented.
- Route/work secrets and the Travel process-session UUID are independently random, short-lived, memory-only values. Logs omit payloads, private keys, bearer tokens, and route/work secrets; process-memory compromise remains outside the protection boundary.
- Test PKI under ignored `tests/e2e/generated/` is disposable and must never be reused in production.

### Fail-closed and resource boundaries

- Control frames are capped at 1 MiB and logical data payloads at 64 KiB. Public setup, TLS handshakes, route prefaces, route responses, and business `OPEN` frames have deadlines.
- Pending Server work and Relay routes default to 256 entries each; Home and Travel default to 128 active flows. Consumed or expired route IDs cannot be reused.
- Identity mismatch, invalid pins/MACs, unexpected messages, discontinuous TCP offsets, or oversized payloads close the affected operation.
- Server admits only the first process-session UUID for a stable Travel ID. Its 45-second renewable lease is checked together with an active scoped grant before work is allocated.
- Any reachable seed returns the complete Server-authorized Relay directory. TCP Flow state and bounded retransmission buffers survive Carrier replacement while Home and Travel stay alive; UDP does not migrate.
- Workspace Rust code forbids `unsafe`; that does not imply every dependency or the AWS-LC C implementation is free of unsafe/native code.

### Current security limits and operator obligations

- Keep Travel UI and TCP/UDP mappings on loopback whenever possible. The UI server is HTTP, not HTTPS. A remotely bound UI requires a bearer token of at least 32 characters, but the application does not measure token entropy and the token is exposed to interception on an untrusted cleartext network unless an external secure tunnel or TLS reverse proxy is used. Generate a high-entropy random token.
- Remotely bound TCP/UDP mappings do not gain bearer-token authentication. `allow_remote_listen = true` only permits the bind; network access must be restricted by a firewall, VPN, or an application-level protocol.
- A Travel identity may hold several independently revocable grants. Global grants authorize every Home, Home grants authorize one Home, and Service grants authorize one exact `(home_id, service_id, protocol)` business. Relay selects a matching active grant for each route; grants never widen themselves or fall back to another Home.
- Copying a Travel package still copies usable private credentials. First-wins session exclusion prevents a later copy from logging in while the legitimate process keeps renewing its lease, but it does not identify which copy is legitimate. A stolen Travel identity must have every active grant revoked; restoring the legitimate device requires new keys, certificates, and newly signed grants.
- Revocation blocks new authorization immediately. A Travel-side local TCP socket that was already open may remain until its configured Carrier-recovery deadline expires, but a revoked Carrier cannot reattach or resume payload delivery.
- Catalog and Relay-directory integrity are hop-by-hop through management TLS, not end-to-end signed. A compromised Server or Relay can falsify the control data shown to Travel, although Home still refuses an unknown or protocol-mismatched service ID and Travel still requires every contacted Relay to present an allowed certificate identity and key.
- The system does not hide traffic metadata, resist endpoint compromise, guarantee availability against Relay/Server or network denial, provide durable replay state across restart, or claim protection after a CA/private key is stolen.
- The project has not undergone a professional third-party security audit. Deployments should treat this release as an auditable implementation baseline, not as a certified security product.

### Frontend containment

The Travel UI and Home issuer UI are TypeScript built with Vite, precompressed at build time, and embedded with [embedded-spa v0.1.1](https://github.com/tomcatzh/embedded-spa/tree/v0.1.1) plus `rust-embed`. API routes are mounted before the SPA fallback; missing API routes and hashed assets return real `404` responses. Embedded frontend bytes are public client assets and must never contain private keys, route secrets, bearer tokens, or confidential configuration.

## Build and check

Requirements: stable Rust, Node.js/npm, CMake, Clang, and Perl. Docker is additionally required for E2E and Linux release artifacts.

```bash
make check
make test
make e2e
make openwrt-ipk
```

`make e2e` generates two temporary test CAs, builds the Linux applications, starts two Relays, two Home Agents, Server, Travel, and TCP/UDP echo targets, and validates:

- encrypted Travel-local enrollment, password-gated Home issuance, transactional Home/Travel private-key password rotation, all three grant scopes, one-year and exact 30-minute validity, and live persistent revocation;
- management and business mTLS, single-use HMAC route admission, TCP/UDP data, TLS-1.2 rejection, TLS-1.3 acceptance, and slow-frame deadlines;
- exact two-Home logical-business routing, same-service-ID isolation, no cross-Home fallback, and independent Home removal/rejoin;
- complete two-Relay discovery from one seed, concurrent Carrier competition, duplicate-process rejection, periodic reevaluation, and same-socket handover after killing the selected Relay;
- both embedded SPAs, gzip/Brotli representations, ETags, cache behavior, and real API/asset `404` boundaries.

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

`9081` is the example configuration's port; the authoritative value is `[issuer].listen` in the
Home configuration. The listener is loopback-only by default. When administering another Home,
use a trusted local tunnel rather than exposing this page directly to WAN.

Upload `enrollment-request.json`, select the validity period, and choose the narrowest suitable
scope:

- **Specified service** authorizes one exact `(home_id, service_id, protocol)` business;
- **Current Home** authorizes all current and future services published by that Home;
- **Global super authorization** authorizes every Home and appears only on a designated Home that
  holds the separately configured global authority.

Enter the **Home issuer-key password** and select **签发并下载结果 (Issue and download)**. This
password decrypts the Home's CA and authority keys; it is not the Travel private-key password or a
public key. FlowSplice uses it for that signing operation and does not persist it.

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

### 5. Rotate private-key passwords

Both password-rotation controls are available only when the corresponding UI listener is bound to
loopback. They are deliberately unavailable through a remotely bound HTTP UI, even when that UI has
an administrator bearer token.

On Home, open `http://127.0.0.1:9081`, select **更改密码 (Change password)** under **Home 签发密码
(Home issuer password)**, then enter the current password and the new password twice. One operation
re-encrypts the management CA key, business CA key, Home authority key, and configured global
authority key. It does not change their public keys, certificates, signed grants, or running business
flows.

On Travel, open its local UI (the example uses `http://127.0.0.1:9080`) and select **Change password**
under **Travel private-key password**. One operation re-encrypts both Travel private keys. The running
process and existing flows continue using the already loaded key material; enter the new password the
next time Travel Agent starts.

FlowSplice never writes either password to macOS Keychain or another password store. Save the new
password separately before confirming. Every key is decrypted and verified before replacement. New
encrypted files are staged in the same directory and switched by atomic rename. A password-free
recovery journal containing only file names and encrypted-file hashes lets the next process finish
an interrupted multi-file switch.

### 6. Revoke access

Return to the same Home issuer page, find the credential, and select **撤销 (Revoke)**. Revocation is
irreversible and is distributed through Server without restarting Server, Relay, or Home. It blocks
new authorization immediately; an already-open local TCP socket closes by Travel's configured
recovery deadline if no authorized Carrier can reattach. A stolen Travel identity may have several
grants, so revoke every active grant bound to that ID and enroll fresh keys.

## Configuration

Server, Relay, Home Agent, and Travel Agent accept `--config <path>` or `FLOWSPLICE_CONFIG`.
Server and Relay also accept `--check-config` for side-effect-free configuration validation. Example
files live beside each application:

- [server/config.example.toml](server/config.example.toml)
- [relay/config.example.toml](relay/config.example.toml)
- [homeagent/config.example.toml](homeagent/config.example.toml)
- [travelagent/config.example.toml](travelagent/config.example.toml)

The E2E certificate generator is disposable test tooling only. Production Travel identities use `flowsplice-travelagent enroll-init` and `enroll-import`; issuance and revocation are performed through the selected Home Agent's separate local UI/API. Operators must provision and protect the Home issuer's encrypted management/business CA keys, Home authority key, optional global authority key, non-Travel leaf keys, renewal process, and SPKI allowlists. Server, Relay, and OpenWrt configs contain only the trusted authority records and public keys. Startup fails when required trust or authorization state is missing or malformed.

The Travel UI and local mappings bind to loopback by default. A non-loopback UI requires `allow_remote_listen = true` and an administrator bearer token of at least 32 characters. Private-key password rotation remains disabled on non-loopback UI listeners because the built-in UI uses HTTP.

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

## Current limits

- TCP Carrier handover is in-memory and requires Home and Travel to stay alive. Restarting either endpoint destroys one endpoint socket and ends the Flow. UDP associations do not migrate.
- Periodic Relay reevaluation backs off from 60 seconds to 15 minutes only while the current Carrier keeps winning. A different winner, timeout, reset, EOF, or TLS/I/O failure restarts competition at the initial interval.
- Homes are independent logical businesses, not failover replicas. A Flow never changes its selected `(home_id, service_id, protocol)`.
- Unattended signing, automated certificate rotation, CRL/OCSP, hardware-backed keys, cross-process Flow recovery, and GA compatibility guarantees are not implemented.

## License

[MIT](LICENSE)
