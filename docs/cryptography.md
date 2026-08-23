# Cryptographic Design and Implementation

This document describes the cryptographic philosophy, concrete primitives, key hierarchy,
authenticated objects, verification order, persistent security state, and current limitations of
FlowSplice 0.1. It documents the implementation in this repository; it is not a formal proof,
professional security audit, compliance statement, or claim that the software is generally
available.

For connection topology, routing, and Carrier behavior, see [Architecture](architecture.md). For the
finding-by-finding record of the current security review, see
[Security audit remediation](security-audit-remediation-2026-08-17.md).

## Design philosophy

### The design is public; only secrets are secret

FlowSplice assumes that an attacker can read the source, protocol, certificate profiles, topology,
logs, and failure behavior. Security must come from protected private keys, fresh random secrets,
authenticated state, and explicit authorization—not from an undocumented protocol or hidden Relay
address.

This principle also defines what may be distributed safely. Certificates, CA certificates, public
keys, signed trust documents, signed credentials, and a deployment-root public key are public
material. Private keys, passwords, bearer tokens, and short-lived route/work secrets are not.

### One configured bootstrap trust point, not executable deployment data

A Travel package contains separate `travel-bootstrap.toml`, `deployment-root.pub`, and
`deployment-trust.json` files. The binary contains none of those deployment values. The
root public-key file verifies a signed
deployment-trust document, which in turn binds the management and business CAs, Server control keys,
Home endpoint keys, and scoped Travel authorities. Relay identities and Home SPKIs are learned from
authenticated state instead of being copied into each Travel configuration.

The configured Seed Relay is therefore transport, not authority. Reaching a Seed cannot change what
Travel trusts. The Management CA obtained from the verified signed trust prevents first contact from
becoming trust-on-first-use; the configured deployment root remains the authority for the returned
complete trust state.

### Separate keys and channels for separate powers

FlowSplice deliberately separates:

- management identity from business identity;
- management TLS from end-to-end business TLS;
- CA certificate issuance from application authorization;
- deployment-root signing from day-to-day Server and Home signing;
- public route identifiers from the secrets that admit sockets to those routes.

A management certificate alone is not a business credential. A Travel certificate pair alone is not
authorization. A signed authorization alone is useful only for the exact Travel keys, identity,
scope, deployment, CAs, certificates, and validity that it binds.

### Authenticate the whole intent

Security-sensitive signed objects carry a version and object type and bind all fields that define the
decision. Enrollment binds the original request and nonce, both Travel public keys, both CAs, both
leaf certificates, authority, scope, deployment trust, credential ID, and validity as one intent.
The Travel-visible control snapshot binds the complete Relay directory and filtered Catalog to one
Travel identity and one generation.

This prevents a valid part from one request, deployment, Home, certificate, or protocol version from
being spliced into another valid-looking result.

### Authenticity includes freshness

A valid signature proves who signed an object; by itself it does not prove that the object is the
latest one. FlowSplice therefore combines signatures with deployment IDs, signer epochs, monotonic
generations, bounded validity, a same-generation content hash, and durable high-water state.

The implementation fails closed on expired control state. It does not promise unlimited offline
operation and simultaneously promise that an offline client has the latest revocation or topology.

### Cryptography protects integrity and confidentiality, not availability

Relay can always delay, drop, reorder, replay, or corrupt opaque business bytes. Server can deny or
misdirect new route setup but is not on the established business path. A compromised Server control
key can sign an incomplete or split view. TLS and signatures make unauthorized changes detectable;
they cannot compel a component to answer or prove to one isolated client that no other client
received a different signed view.

## Protection layers

| Layer | Endpoints | Construction | Security purpose |
| --- | --- | --- | --- |
| Management TLS | Home→Server, Server→Relay, Travel→Relay | Mutual TLS 1.3 under the management CA, followed by application role/ID/SPKI/grant checks | Authenticates control peers and protects catalogs, heartbeats, authorization state, and route/work-secret delivery |
| Route admission | Travel→Relay and Home→Relay data sockets | HMAC-SHA256 over a fixed preface with an independent random 32-byte secret | Proves that a socket owns one allocated route or work item before Relay pairs it |
| Business TLS | Travel↔Home through one Relay | End-to-end mutual TLS 1.3 under the business CA, followed by Home/Travel SPKI and grant checks | Protects the selected service ID, logical Flow frames, acknowledgements, and business plaintext |
| Signed deployment state | Offline root→all components | ECDSA P-256/SHA-256 over exact serialized deployment-trust bytes | Binds every lower-level trust domain to one deployment root |
| Signed authorization | Home authority→Server/Relay/Home | ECDSA P-256/SHA-256 over an exact Travel credential | Grants the bound Travel keys one explicit Global, Home, or Service scope |
| Signed discovery state | Server→Travel | ECDSA P-256/SHA-256 over an atomic Relay-directory and filtered-Catalog snapshot | Prevents a Seed or Relay from forging discovery or Catalog contents |
| Signed statistics | Travel/Relay/Home→Server | ECDSA P-256/SHA-256 over exact five-minute summaries, bound to the reporter's management certificate and live role/session | Authenticates node-observed business aggregates for idempotent global collection |

The outer management and route layers do not replace the inner business TLS. Relay forwards the
business TLS stream but does not possess the Travel or Home business private keys. Server forwards
only control messages and signed summaries.

## Concrete primitives and libraries

`Cargo.lock` is the exact dependency-version authority. At the time of this document, the relevant
resolved versions are:

| Use | Current implementation |
| --- | --- |
| TLS | `rustls` 0.23.43 through `tokio-rustls` 0.26.4, with the AWS-LC provider |
| Cryptographic provider | `aws-lc-rs` 1.18.0 |
| TLS versions | TLS 1.3 only; TLS 1.2 is not enabled |
| Detached signatures | ECDSA over NIST P-256 with SHA-256; ASN.1 DER signatures |
| Digests and SPKI pins | SHA-256, encoded as 64 lowercase hexadecimal characters |
| Route/work admission | HMAC-SHA256 with independent 32-byte random keys |
| Enrollment and authority keys | P-256 keys encoded as PKCS#8 |
| Password-protected private keys | Encrypted PKCS#8 using PBES2, scrypt `log2(N)=17, r=8, p=1`, and AES-256-CBC |
| Randomness | AWS-LC `SystemRandom` for protocol secrets/nonces/signatures; operating-system `OsRng` for encrypted-PKCS#8 salt and IV generation |
| Certificate construction/parsing | `rcgen` 0.14.9, `rustls-pki-types`, and `x509-parser` 0.18.1 |
| Secret cleanup | `zeroize`/`Zeroizing` for passwords and selected private-key buffers |

Every TLS configuration is built with `TLS13` as the only protocol version. The project does not
select a custom TLS cipher suite or reimplement the TLS key schedule; those operations are delegated
to rustls and its AWS-LC provider. Every executable installs that provider during startup through
`flowsplice_core::init_crypto`. The code does not enable TLS 0-RTT. It leaves rustls's default
in-memory TLS 1.3 session-resumption stores enabled; those stores are not persisted or shared across
processes and do not replace any FlowSplice identity, SPKI, or grant check.

The project does not implement a custom encryption algorithm, hash construction, signature scheme,
or random-number generator. It also does not enable a FIPS feature or make a FIPS claim.

Deployment-root, Server-control, and Travel-authority public keys are encoded as a 65-byte
uncompressed SEC1 P-256 point (`04 || X || Y`) and then hexadecimal. SPKI pins instead hash the full
DER `SubjectPublicKeyInfo` and are always exactly 32 SHA-256 bytes.

## Key and capability inventory

| Material | Private material resides at | Public binding or distribution | Power and rotation boundary |
| --- | --- | --- | --- |
| Deployment root | Offline, password-encrypted `deployment-root.key` | Public point is a separate configured trust file for Home/Travel/Server/Relay | Can replace every subordinate trust domain; changing it requires an independently authenticated configuration migration, never an executable rebuild |
| Management CA | Issuer-side encrypted private key; CA certificate is public | Exact CA certificate is root-bound in deployment trust and carried in enrollment responses | Issues management certificates; compromise does not by itself create a valid Travel grant, but can impersonate roles where later SPKI/grant checks do not narrow the certificate |
| Business CA | Issuer-side encrypted private key; CA certificate is public | Exact CA certificate is root-bound in deployment trust and carried in enrollment responses | Issues business certificates; Travel/Home still apply root-bound Home SPKI or signed Travel-grant checks |
| Server control-signing key | Server runtime as an unencrypted PKCS#8 file protected by host/file permissions | Public key, Server ID, and epoch are root-bound | Signs Travel-specific Relay/Catalog snapshots; compromise controls discovery integrity and availability until its epoch is removed |
| Server/Relay/Home management TLS keys | Their respective runtime hosts | Management-CA certificates plus configured or root-derived identity checks | Authenticate management links; key rotation must preserve or update the corresponding pins/trust |
| Home business TLS key | Home runtime | Business certificate plus root-bound Home business SPKI set | Terminates business TLS for that Home; compromise exposes that Home's plaintext |
| Home Travel authority | Issuing Home, password-encrypted | Public key, epoch, owner Home, and role are root-bound | May issue Home- or Service-scoped grants only for its own Home |
| Global Travel authority | One explicitly designated issuing Home, password-encrypted | Public key, epoch, owner Home, and Global role are root-bound | May issue grants for every Home; it is intentionally higher privilege |
| Travel management key | Travel device, password-encrypted | Management leaf certificate and signed credential SPKI | Authenticates Travel control connections; never uploaded during enrollment |
| Travel business key | Travel device, password-encrypted | Business leaf certificate and signed credential SPKI | Terminates end-to-end business TLS; never uploaded during enrollment |
| Route secret | Relay memory, delivered to one Travel over management TLS | Never persisted or published | Single-use admission to one Relay route; default lifetime 15 seconds |
| Work secret | Created by Server, delivered to one Relay and one Home over management TLS, then held pending by Relay | Never persisted or published | Single-use pairing of Home and Travel data sockets at that Relay; default lifetime 15 seconds |
| Travel process-session UUID | Travel memory | Presented only after Travel mTLS authentication | Supplementary 45-second first-live-session capability; not a replacement for credential revocation |

The deployment-root public key is not a credential. Stealing a Travel binary or copying that public
key provides verification capability only.

## X.509 identity model

Management and business use separate CA roots. Every accepted FlowSplice leaf certificate must
contain exactly one FlowSplice identity URI SAN of the form:

```text
flowsplice://identity/<role>/<stable-id>
```

Other SANs do not become FlowSplice identities. The recognized roles are `server`, `relay`, `home`,
and `travel`. After the TLS implementation
validates the certificate path, signature, validity, and client/server EKU, FlowSplice parses the leaf
certificate and requires:

1. exactly one FlowSplice identity URI;
2. the expected role;
3. the expected stable ID where the relationship names one;
4. the expected SHA-256 SPKI where a configured or signed allowlist applies;
5. for Travel, an active signed credential binding that stable ID and the exact management or
   business SPKI;
6. for business access, the exact routed credential ID and its authorization scope.

Home→Server uses normal certificate-chain and configured DNS/IP server-name verification, then checks
the Server role, stable ID, and configured Server SPKI. Relay also pins Server. Connections for which
FlowSplice identity is intentionally independent of DNS—Server→Relay discovery, Travel→Relay, and
Travel→Home business TLS—validate the CA chain first, then use the exact URI role/ID and SPKI checks
above. The SNI value used for those connections is not treated as endpoint identity.

SPKI pins hash the leaf certificate's DER `SubjectPublicKeyInfo`, not the complete certificate. A
certificate may therefore be renewed with the same key without changing its pin. A key rotation
requires an overlap or trust update containing the new SPKI.

## Deployment root and signed trust

`flowsplice-trust root-init` generates a P-256 root key, writes the encrypted private key with mode
`0600`, writes the public point with mode `0644`, and refuses to replace an existing output
directory. The private key password must contain at least 12 characters and is read from a hidden
terminal prompt in normal operation.

The root signs the exact compact JSON bytes of a `DeploymentTrust` payload. The signed container
stores those bytes as `payload_hex` and the DER ECDSA signature as `signature_hex`. Verification
checks the signature over the exact bytes before parsing the payload. The format does not depend on
re-serializing arbitrary JSON into a canonical form.

The signed payload binds:

- format version;
- deployment ID;
- monotonic trust generation;
- not-before and not-after times;
- the exact management CA certificate;
- the exact business CA certificate;
- every permitted `(server_id, control-key epoch, public key)` tuple;
- every Home ID and its management/business SPKI sets;
- every Travel authority ID, epoch, owner Home, public key, and Global/Home role.

Shape validation rejects an empty deployment, generation zero, invalid validity interval, malformed
or duplicate Server epochs, duplicate Home IDs, empty Home pin sets, malformed pins, duplicate
authority IDs, and authorities whose owner is not a trusted Home. Verification allows at most five
minutes of not-before clock skew and rejects an expired trust document.

The root private key is never a Server, Relay, Home, or Travel runtime input. It is needed only to
create or renew deployment trust. A higher-generation document signed by the same root can update
the trust without rebuilding Travel; changing the root requires a new binary or another independent
trusted channel carrying the replacement root.

## Travel enrollment and atomic authorization

### Request creation

The normal `enroll-remote` bootstrap creates a new mode-`0700` directory and refuses conflicting
reuse. It generates distinct P-256 management and
business keys and write both as mode-`0600`
password-encrypted PKCS#8 files. They also generate:

- a random request UUID;
- an independent 32-byte enrollment nonce;
- a CSR for each key proving possession of that key;
- a local state file containing the request identity and both SPKI hashes.

Each CSR is restricted to ECDSA P-256/SHA-256 and exactly one Travel identity URI. The two CSRs must
not contain the same public key. Requests more than seven days old or more than five minutes in the
future are rejected.

Only `enrollment-request.json` is public input to an issuer. The encrypted keys and
`enrollment-state.json` stay on the Travel device.

For first remote enrollment, Travel additionally creates a random 32-byte retrieval token and keeps
it in a mode-`0600` resumable bootstrap record. The token and exact request derive the short code
shown independently by Travel and Home. The anonymous TLS client is restricted to bootstrap submit
and poll messages, the request remains subject to CSR proof-of-possession and freshness checks, and
Home still performs local attended password-gated signing. A bearer of the retrieval token may read
the public enrollment response, but cannot derive either Travel private key or sign another
credential. The token record is removed after verified installation.

### Home-side issuance

The Home issuer verifies the root-signed deployment trust, request freshness, both CSR proofs of
possession, selected authority, authority scope, validity, exact CA certificates, and that every
loaded private key matches its root-bound public material. It constructs the certificate extensions
locally rather than copying untrusted CSR extensions.

The issued management and business certificates are client-auth leaves with the same exact Travel URI
identity, separate public keys, and the approved validity. Validity defaults to 365 days, is capped at
3650 days, and cannot extend outside the deployment-trust window.

The selected Home authority signs one `TravelCredential` that binds:

```text
version + object_type
deployment_id + SHA-256(deployment-trust payload)
credential_id
authority_id + authority_epoch
enrollment_request_id + 32-byte nonce + SHA-256(complete request)
travel_id
management SPKI + business SPKI
SHA-256(management CA) + SHA-256(business CA)
SHA-256(management leaf certificate) + SHA-256(business leaf certificate)
authorization scope
not_before + not_after
```

The payload's exact JSON bytes are hex-encoded beside a DER ECDSA signature. A Home authority may sign
only its own Home or Service scope; only a root-declared Global authority may sign Global scope.

### One response and import

The self-contained response contains the approval, root-signed deployment trust, both Travel leaf
certificates, and the signed credential. Travel needs no separately entered CA, authority public key,
Relay ID, or Relay SPKI.

Import performs these checks in order:

1. the response and approval versions/IDs are valid;
2. the response carries the exact original request;
3. the response deployment trust verifies against the root selected by the explicit bootstrap configuration and does not roll back or conflict with the configured baseline trust;
4. the selected authority is present in that trust and permits the scope;
5. the authority signature and every atomic credential field match the approval, CAs, certificates,
   and request;
6. both certificates have the expected Travel identity, SPKI, and validity and chain to their exact
   root-bound CA;
7. both encrypted local private keys decrypt with the entered Travel password and match the local
   state and issued certificates;
8. every installed file is new or byte-for-byte identical to the existing file.

This makes import idempotent but prevents a later response from replacing an enrolled identity in
place.

### Single-use issuance ledger

The issuer computes SHA-256 over the request's deterministic struct serialization and persists a
mode-`0600`, atomically replaced ledger before publishing the credential. An identical retry with the
same authority, scope, and validity returns the byte-identical response or completes an interrupted
publication. Changing those decisions is rejected. Revocation or expiry does not make the request
reusable, and reusing one request ID with different content is rejected.

The ledger is application state, not a cryptographic signature. Its purpose is to make the issuer's
single-use policy durable and idempotent.

## Signed Relay directory and Catalog

After authenticating a Travel management certificate and its active grants, Server constructs one
Travel-specific `ControlSnapshotPayload` containing:

- version and object type;
- deployment ID;
- Server ID and control-key epoch;
- exact Travel ID and management SPKI;
- monotonic generation;
- issue and expiry times;
- the complete Relay directory, including each Relay ID, management address, public data address, and authenticated management SPKI;
- the aggregate Catalog filtered by all active grants for that Travel identity.

Server signs the exact payload bytes with its root-certified P-256 control key. The snapshot also
carries the root-signed deployment trust. The configured snapshot lifetime must be nonzero and no
more than five minutes; the example uses two minutes. Verification rejects a snapshot issued more
than five minutes in the future or one that has expired.

On Server startup, the next snapshot generation begins with the current Unix time in nanoseconds and
then increments in memory. A new Server key epoch is the explicit escape from the prior epoch's
generation space. Travel's durable high-water state rejects a restarted or clock-regressed Server
whose signer epoch/generation would move backward.

Before applying a snapshot, Travel verifies:

1. root signature and deployment-trust validity;
2. Server control-key membership for the exact Server ID and signer epoch;
3. Server signature over the exact snapshot bytes;
4. snapshot deployment ID, object type, validity, Directory/Catalog shape, and unique entries;
5. exact Travel ID and management SPKI subject;
6. configured Home IDs against root-bound Home endpoints;
7. the connected Relay's URI ID and SPKI against an entry in the signed directory;
8. durable trust/signer epoch and generation rules.

Travel commits `deployment_id`, trust generation/digest, signer epoch, highest snapshot generation,
that generation's unique content hash, and the complete signed snapshot to redb with immediate
durability. A lower trust generation, lower signer epoch, or lower snapshot generation is rejected.
The same generation with different bytes is a hard failure. Cached state is reused only while its
signatures and expiry remain valid; expiry never fails open. Independently, Travel retains every Relay
from a verified directory as a historical startup candidate. That history can help reconnect after a
restart but cannot authorize business use: a fresh signed snapshot must authorize the Relay first. A
legacy `control-trust-state.json` is verified, committed to redb, read back and digest-checked, then
deleted.

There is currently no `previous_state_hash` chain and no transparency log, witness, gossip, or quorum.
A certified Server control key can sign different same-generation or different-generation views for
different Travel devices. Each device detects conflict against its own history, not equivocation
between devices.

## Route and work admission

Route and work IDs are UUIDs and are not secret. Possession of an ID alone grants nothing.

Server creates a 32-byte random work secret for each pending work item and sends it over independent
management-mTLS links to the selected Relay and exact Home. Relay creates a separate 32-byte random
route secret and sends it to Travel over management TLS.

The joining data socket writes this fixed authenticated preface:

```text
magic "FSLCRTE2" (8 bytes)
side                 (1 byte: Travel or Home)
route/work UUID     (16 bytes)
HMAC-SHA256         (32 bytes over the preceding 25 bytes)
```

Relay checks the fixed length, magic, side, UUID, expected side, and HMAC. It consumes the named
Travel-route or Home-work half on the first connection attempt, making each half single-attempt as
well as single-use, and pairs a socket only after both halves authenticate. Server has no data socket
or pending data listener. Secrets must be exactly 32 bytes. Relay's pending entries expire after a
short configured lifetime and are process-local. An attacker who can learn a live route/work ID and
reach Relay may consume that pending half with an invalid attempt, causing denial of service but not
successful admission.

This HMAC authenticates socket admission only. It does not encrypt the data socket, authenticate the
business endpoint, or protect bytes after the preface. Those properties come from the inner
Travel↔Home business TLS.

## End-to-end business TLS and authorization

After Relay pairs the opaque Travel and Home sockets, Travel and Home complete a mutual TLS 1.3
handshake through that Relay. Rustls may resume an earlier in-process TLS session, but FlowSplice still obtains the
authenticated peer identity and applies the current SPKI/grant checks. Travel requires the
certificate to contain the selected Home ID and a business SPKI present in root-signed deployment
trust. Home resolves the Travel certificate's ID and business SPKI through the current
signed-credential set and requires the exact credential ID chosen for the route.

Only after business TLS succeeds does Travel send the encrypted `OPEN` carrying
`(flow_id, home_id, service_id, protocol, race_id, carrier_id, offsets)`. Home checks that:

- the `home_id` is itself;
- the service/protocol exists locally;
- the routed credential is active;
- the same credential permits that exact Home/service/protocol.

The service ID is therefore unavailable to Relay and Server even though the Home ID is routing
metadata. Logical TCP `DATA`, `ACK`, `DUP`, `FIN`, and Carrier-race frames remain inside this business
TLS stream. Carrier replacement establishes another business TLS connection to the same Home and
reattaches to the in-memory logical Flow. TLS resumption may optimize its handshake, but cannot
resurrect a TCP socket or Flow after Travel/Home process restart.

## Authorization distribution and revocation

Credentials are individually signed by deployment-trusted Home/global authorities. Server verifies a
publishing Home's management identity, requires that Home to own the named authority, verifies the
credential and scope, persists the add-only credential set, and advances a monotonic authorization
generation.

Revocation also originates over the owning Home's authenticated control session. Server verifies
ownership, persists the credential ID, timestamp, and reason, then distributes a new authorization
snapshot to Relay and every Home over their pinned management-TLS sessions.

The authorization snapshot is not separately signed by the deployment root or Server control key.
Its transport authenticity comes from the authenticated Server channel; each credential inside it
retains its Home-authority signature. Relay and Home verify every credential and durably persist the
highest authorization generation plus all observed revoked IDs. They reject a lower generation and
any snapshot that removes a previously observed revocation.

This design lets revocation take effect without restarting Server, Relay, or Home. It also means a
compromised pinned Server can suppress service, falsely add revocations, or freeze delivery until
cached authorization becomes operationally unusable. It still cannot create a new active grant
without a trusted Home/global authority signature.

An expired or revoked credential cannot authenticate a new Travel management session, allocate a
route, or attach a business Carrier. Active state using that credential is closed. Revocation is
irreversible; replacement requires a fresh enrollment request, keys, certificates, and credential ID.

## Private-key encryption and password rotation

Deployment-root, issuer CA/authority, and Travel private keys use encrypted PKCS#8 PEM. The current
`pkcs8` crate profile derives a 256-bit encryption key with scrypt parameters `N=131072`, `r=8`, and
`p=1`, using a fresh random 16-byte salt, then encrypts with AES-256-CBC and a fresh random 16-byte IV.
FlowSplice's interactive root/Travel creation and password-rotation paths require at least 12
characters. The decoder can load externally provisioned material with any correct nonempty password,
so 12 characters is an operator-workflow floor, not an on-disk format property or a claim of
sufficient entropy.

Passwords are read through hidden terminal input or submitted for one UI/API operation. Production
UI listeners require an explicit `127.0.0.1` address; there is no remote-listen configuration gate.
Passwords are not stored in macOS Keychain, configuration, the issuer ledger,
or the rotation journal. Backend password strings and selected decoded private-key buffers use
`Zeroizing`.

Runtime private-key files are opened with no-follow semantics and checked on the opened descriptor:
they must be regular files owned by the effective service user, have no group/other permission bits,
and be no larger than 1 MiB. This prevents symlink substitution, FIFO blocking, and silently using a
key exposed through broad file permissions.

Password rotation does not rotate a public key. It:

1. requires the current password and a different new password;
2. decrypts every key in the configured group before changing anything;
3. re-encrypts every unchanged PKCS#8 private key with fresh salt and IV;
4. writes and reopens mode-`0600` staged files to verify the new password and identical DER key;
5. publishes a password-free journal containing only file names and encrypted-file hashes;
6. replaces each file by same-directory atomic rename and fsyncs the directory;
7. completes a safely journaled interrupted switch on the next startup.

All keys in one rotation group must be regular files in the same directory. Home rotates its
management CA, business CA, Home authority, and optional global authority together. Travel rotates
its management and business keys together.

Password rotation only re-encrypts the same key material. It does not invalidate an encrypted copy
stolen before rotation, change certificate validity, revoke a grant, or remove a key already loaded
by a running process. Suspected key theft requires the relevant key/certificate/authority epoch or
Travel credential to be replaced or revoked; changing only the wrapping password is insufficient.

PBES2 with AES-256-CBC is not an AEAD construction and does not carry an independent authentication
tag. Wrong passwords and corrupted ciphertext are normally rejected by decryption, PKCS#8 parsing,
and subsequent public-key verification, but this at-rest format should not be described as
authenticated encryption. A future format change may choose an authenticated private-key envelope.

## Persistent state and rollback behavior

| State | Persistence and rule |
| --- | --- |
| Travel deployment/control state | redb with immediate durability; binds deployment/trust digest, signer epoch, highest generation, same-generation hash, and cached signed snapshot; verified legacy JSON migrates once |
| Travel Relay history | redb; retains every Relay from a verified directory as a startup candidate, never as permanent authorization |
| Local business statistics | Travel/Relay/Home redb stores five-minute buckets and a durable signed-report outbox; only locally observed business metrics are recorded |
| Server collected statistics | redb; verifies certificate/role/session-bound signatures and idempotently deduplicates node summaries by revision/digest |
| Remote enrollment | A temporary Travel bootstrap record makes first enrollment resumable; after installation, Travel redb outbox and Home redb inbox preserve request/response/install-ack lifecycle across reconnect and restart |
| Relay/Home authorization cache | Explicit one-time initialization plus atomic JSON updates; missing/invalid state fails closed, and accepted state rejects lower generations, same-generation content changes, and loss of any observed revocation |
| Server authorization state | One atomic JSON object containing generation, add-only credentials, irreversible revocations, and permanently spent enrollment-request hashes |
| Server control generation | Atomic JSON high-water mark reserved before each signed snapshot; gaps are allowed but reuse after restart is not |
| Home issuance ledger | Mode-`0600` atomic file; makes enrollment requests permanently single-use and retry-idempotent |
| Password rotation journal | Mode-`0600`, password-free, hash-bound recovery record removed after all replacements complete |
| Pending route/work secrets | Memory only, single-use, bounded, and short-lived; lost on process restart |
| Travel session lease | Server memory only; 45-second crash/network-partition fallback |

redb commits use immediate durability for security-sensitive state and durable outboxes. The
remaining JSON stores use `store_json_atomic`, which writes a new file, fsyncs it, renames it over the
target, then fsyncs the parent directory. These stores provide crash-safe local behavior; they are
not a replicated consensus log and do not defend against an attacker who can arbitrarily rewrite that
host's files.

Travel durably prevents deployment-trust rollback. Server, Relay, and Home verify their configured
root-signed trust at startup and stop once it expires; they do not currently maintain an independent
durable high-water record for deployment-trust generations. Protecting or deliberately replacing
their trust files remains an operator responsibility.

## Compromise boundaries

| Compromised material or component | Consequence |
| --- | --- |
| Deployment-root private key | Catastrophic for the deployment: an attacker can authorize replacement CAs, Server keys, Home endpoints, and Travel authorities |
| Global authority key | Can grant any Travel keys access to every Home while the authority epoch remains trusted |
| One Home authority key | Can mint grants for that Home, but not another Home or Global scope |
| Management CA key | Can issue management certificates; later role/ID/SPKI/grant checks still apply, but relationships without a separately pinned peer key can be impersonated |
| Business CA key | Can issue business certificates; root-bound Home SPKIs and signed Travel grants still constrain endpoint acceptance |
| Server control key | Can forge, omit, or equivocate Relay/Catalog views and control path availability; cannot impersonate a root-bound Home business key |
| Pinned Server runtime | Can observe/control routing metadata, misdirect or deny new routes, distribute revocations, and alter collected statistics; it is not on an established business stream and cannot decrypt business TLS without an endpoint key |
| Relay | Can observe metadata and delay/drop/corrupt opaque traffic; cannot forge signed control state or decrypt business TLS |
| Home host | Exposes that Home's services and plaintext plus every issuer key actually stored there; a designated global issuer has correspondingly global authorization power |
| Travel host | Exposes that device's local plaintext, private keys while usable, mappings, and every scope granted to it |
| Final target service | Exposes plaintext after Home terminates business TLS |

If the deployment root is compromised, recovery cannot safely rely only on the old root signing a new
root. It requires an independent trusted software/configuration channel, such as a verified new
Travel build, and reissuance under the replacement deployment.

## Deliberate non-goals and current limitations

- FlowSplice does not hide IP addresses, timing, packet/record sizes, byte counts, selected Home ID,
  or the existence of control relationships.
- Server is the final authority for the aggregate Catalog/Relay view. There is no transparency log,
  gossip, witness, quorum, or Home-signed Catalog aggregation to detect Server split views.
- There is no signed `previous_state_hash` chain between control snapshots.
- Application Travel revocation is not CRL or OCSP. Automatic CA, Server, Home, or authority-key
  revocation/rollover orchestration is not implemented.
- The deployment root is one password-encrypted software key. Threshold recovery and hardware-backed
  root custody are not implemented.
- Non-Travel runtime TLS keys and the Server control key are not automatically encrypted or backed by
  an HSM. Their filesystem and host protection are deployment responsibilities.
- The built-in Home, Travel, Relay, and Server UIs use HTTP and require exact loopback listeners.
  Remote administration requires a separately authenticated and encrypted tunnel.
- Password-protected private keys are not locked with `mlock`, and the implementation cannot promise
  that every compiler/library/OS copy of sensitive memory is erased.
- Route/work secrets are currently protocol `Vec<u8>` values that may be cloned and are not
  comprehensively zeroized after use.
- A compromised endpoint sees plaintext at that endpoint. Protocol cryptography cannot protect a
  Travel, Home, or target process from its own host administrator or malware.
- Signed state protects integrity and rollback only within the persisted history available to that
  component. Deleting or maliciously replacing local high-water files defeats that local history.
- Release binaries are not yet Developer ID notarized, reproducibly attested, or delivered by an
  authenticated anti-rollback updater. Separate configuration prevents deployment coupling but does
  not by itself authenticate the adjacent root file; package provenance or an independently verified
  root fingerprint remains required.
- The project has not undergone a professional third-party security audit and is not a certified
  security product.

## Implementation map

| Area | Primary implementation |
| --- | --- |
| TLS construction, URI identity, SPKI extraction/checking | [`crates/flowsplice-core/src/tls.rs`](../crates/flowsplice-core/src/tls.rs) |
| HMAC route/work preface | [`crates/flowsplice-core/src/route.rs`](../crates/flowsplice-core/src/route.rs) |
| Deployment trust and Server control snapshots | [`crates/flowsplice-core/src/deployment.rs`](../crates/flowsplice-core/src/deployment.rs) |
| Scoped Travel credentials, revocations, authorization cache | [`crates/flowsplice-core/src/authorization.rs`](../crates/flowsplice-core/src/authorization.rs) |
| Five-minute statistics payloads and signatures | [`crates/flowsplice-core/src/statistics.rs`](../crates/flowsplice-core/src/statistics.rs) |
| redb state, statistics buckets/outboxes, history, and deduplication | [`crates/flowsplice-storage/src/lib.rs`](../crates/flowsplice-storage/src/lib.rs) |
| Enrollment request/import and certificate validation | [`crates/flowsplice-enrollment/src/lib.rs`](../crates/flowsplice-enrollment/src/lib.rs) |
| Private-key encryption and transactional password rotation | [`crates/flowsplice-enrollment/src/key.rs`](../crates/flowsplice-enrollment/src/key.rs) |
| Home certificate/credential issuance | [`crates/flowsplice-enrollment/src/issuer.rs`](../crates/flowsplice-enrollment/src/issuer.rs) |
| Offline root utility | [`crates/flowsplice-enrollment/src/bin/flowsplice-trust.rs`](../crates/flowsplice-enrollment/src/bin/flowsplice-trust.rs) |
| Server credential/revocation store | [`server/src/authorization.rs`](../server/src/authorization.rs) |
| Home single-use issuance ledger | [`homeagent/src/issuance_ledger.rs`](../homeagent/src/issuance_ledger.rs) |
| Travel durable control state and authenticated remote enrollment | [`travelagent/src/main.rs`](../travelagent/src/main.rs) |
| End-to-end regressions | [`tests/e2e/run.sh`](../tests/e2e/run.sh) and [`tests/e2e/assert_e2e.py`](../tests/e2e/assert_e2e.py) |

## Verification coverage

Unit tests cover strict signed-object parsing, P-256 signature verification, SPKI checks, HMAC
tamper rejection, enrollment binding, wrong-password rejection, certificate/CA mismatch, scope
enforcement, authorization rollback, and interrupted password-rotation recovery.

The Docker E2E suite additionally exercises:

- TLS 1.3 acceptance and TLS 1.2 rejection;
- credential-less first remote Travel enrollment, authenticated remote replacement enrollment,
  verification-code comparison, Home password approval, Travel password installation, explicit
  install acknowledgement, and restart activation;
- tampered trust and cross-request/cross-certificate response-splicing rejection;
- independent enrollment nonce binding and single-use/idempotent issuance;
- Global, Home, and Service scopes across two Homes;
- discovery of two Relay SPKIs from one Seed;
- durable Travel control high-water state, one-time legacy JSON migration, and historical Relay
  candidates across restart without stale authorization;
- Home-originated live revocation, cross-Home revocation rejection, and rollback-resistant Relay/Home
  caches;
- rejection of revoked Travel certificates at both Relays;
- Home and Travel password rotation, wrong-password rejection, and recovery across restart;
- same-logical-TCP-flow Carrier handover after killing the winning Relay;
- established-flow continuity while Server is stopped and the absence of any Server business listener;
- five-minute node statistics, signed upload, Server certificate binding and idempotent deduplication,
  role-owned metric rejection, and rolling day/week/month/year report windows.

Passing these tests demonstrates the encoded invariants for the exercised cases. It is not a
cryptographic proof or substitute for independent review.

## Standards and upstream references

- [RFC 8446 — TLS 1.3](https://www.rfc-editor.org/rfc/rfc8446.html)
- [RFC 5280 — Internet X.509 PKI certificate profile](https://www.rfc-editor.org/rfc/rfc5280.html)
- [RFC 2104 — HMAC](https://www.rfc-editor.org/rfc/rfc2104.html)
- [RFC 7914 — scrypt](https://www.rfc-editor.org/rfc/rfc7914.html)
- [RFC 8018 — PKCS #5 / PBES2](https://www.rfc-editor.org/rfc/rfc8018.html)
- [RFC 5958 — PKCS #8 private-key information syntax](https://www.rfc-editor.org/rfc/rfc5958.html)
- [NIST FIPS 186-5 — Digital Signature Standard](https://csrc.nist.gov/pubs/fips/186-5/final)
- [NIST FIPS 180-4 — Secure Hash Standard](https://csrc.nist.gov/pubs/fips/180-4/upd1/final)
- [NIST FIPS 197 — Advanced Encryption Standard](https://csrc.nist.gov/pubs/fips/197/final)
- [`rustls` 0.23.43 documentation](https://docs.rs/rustls/0.23.43/rustls/)
- [`aws-lc-rs` 1.18.0 documentation](https://docs.rs/aws-lc-rs/1.18.0/aws_lc_rs/)
- [`pkcs8` 0.10.2 documentation](https://docs.rs/pkcs8/0.10.2/pkcs8/)
- [`redb` 4.2.0 documentation](https://docs.rs/redb/4.2.0/redb/)
