# Architecture

## Security boundary

FlowSplice has two independent trust domains:

1. Management TLS authenticates Server, Relay, Home, and Travel control links.
2. Business TLS authenticates only Travel and Home and carries logical TCP/UDP frames.

The CAs are intentionally separate. A Relay management key cannot authenticate as Travel to Home. In addition to certification-path and EKU verification, applications require the certificate URI SAN role and stable ID to match the protocol peer. SHA-256 SPKI allowlists narrow Server, Relay, management-Home, and business-Home relationships. An offline ECDSA P-256 deployment root signs one trust document binding the deployment ID and validity, both CA certificates, Server control-key epochs, every Home endpoint's management/business SPKIs, and every Home/global Travel authority with its epoch, role, and scope. The root private key is not available to any runtime daemon; Travel binaries embed only its public key. Travel management and business SPKIs are jointly bound to one or more scoped grants by the root-certified authorities. All TLS configurations explicitly permit TLS 1.3 only and use rustls with the AWS-LC provider.

## Control topology

```text
Home Agent A --outbound mTLS--┐       ┌--outbound mTLS--> Relay A <--mTLS--┐
                              ├--> Server                                  Travel Agent
Home Agent B --outbound mTLS--┘       └--outbound mTLS--> Relay B <--mTLS--┘
```

Each Home publishes its own catalog. Server keeps an explicit configured set of permitted Home IDs, derives each Home's management SPKI key set from the verified deployment trust, keeps one replaceable session per Home ID, and builds a sorted aggregate catalog. Reconnecting one Home supersedes only that ID's old session; disconnecting it removes only its catalog. Server also maintains an isolated reconnect loop and sender for every configured Relay. After a configured Relay address passes management-CA and expected URI role/ID checks, Server extracts its authenticated SPKI and updates the memory-only directory; Relay SPKIs are not duplicated in Server configuration.

Home-issued credential and revocation snapshots are distributed to Relay and Home; both derive the verifying authority set from the root-signed deployment trust, durably apply anti-rollback state, and acknowledge the initial generation before their control session becomes available. Travel-visible state uses a separate end-to-end envelope: for each admitted Travel identity, Server filters the aggregate Catalog by all active grants and signs the complete `RelayDirectory + Catalog + generation + issued_at + expires_at` with a control key certified by the deployment root. Relay forwards this opaque signed snapshot and cannot edit either half or manufacture a new generation.

Server remains the final authority for that aggregate view. A compromised certified Server control key can equivocate, omit entries, or sign a false Directory/Catalog and thereby control path selection and availability. This design does not implement Home-signed Catalog transparency, gossip, witnesses, or a quorum. Root-bound Home endpoint SPKIs, scoped Travel grants, and end-to-end business TLS still prevent Server from impersonating Home or decrypting business payloads. A deployment-root compromise is broader: it can replace every CA, Server key, Home endpoint, and authority. Recovery therefore requires an independent trusted software/configuration channel carrying a new root, never a root transition trusted only because the compromised old root signed it.

A Travel Agent configures only one or more Seed addresses. A Seed is an untrusted transport: after management-CA TLS, Travel still verifies the embedded deployment root, deployment and Server signatures, exact Travel subject, signer epoch, freshness/generation, root-bound Home identities, and that the connected Relay certificate ID and SPKI occur in the signed directory. It durably stores the accepted high-water generation and unique content hash before applying the atomic Directory/Catalog, including across restart, and may reuse the complete cached snapshot only while its signature and expiry remain valid. A malicious Relay may drop, delay, or replay a still-unexpired snapshot, but cannot forge Directory, Catalog, metadata, or path selection. Server and Relay keep control traffic separate from data sockets. Control setup frames have deadlines, and established links are reclaimed after three missed 10-second heartbeat intervals.

At process startup Travel creates a random in-memory session UUID. The first catalog connection for a signed Travel identity acquires a 45-second renewable lease in Server through its Relay. Connections through other Relays are allowed only when they carry the same stable Travel ID and process-session UUID, which is required for multi-Relay route competition. A different process-session UUID for the same Travel ID is rejected globally and cannot displace the active session, even when the identity owns several grants. Server prunes an unrenewed lease, allowing a later process to log in after the old process is no longer demonstrably online.

Server binds exactly one explicit Home-control address shared by all configured Home identities and may bind multiple explicit IPv4 and IPv6 addresses for Relay/Home data pairing. All configured data listeners are bound before their accept loops start, so a partial bind failure fails startup instead of silently exposing an incomplete topology.

## Route and data setup

1. An authenticated Travel control connection presents its admitted process-session UUID and the logical business's Home ID, then asks Relay for an opaque route. It does not reveal a service ID.
2. Relay selects one currently active signed grant for the Travel TLS identity that authorizes the requested Home, then asks Server for work using that credential ID and Home ID.
3. Server selects exactly that configured and online Home, creates a random 32-byte work secret, records the Home ID with a short expiry, and asks only that Home to connect a work socket.
4. Relay creates a separate random 32-byte single-use route secret for Travel.
5. Travel authenticates its Relay data preface with HMAC-SHA256.
6. Relay atomically consumes the route and authenticates its work connection to Server with the Server-issued secret.
7. Home independently authenticates its work connection with the same secret.
8. Server pairs the Home and Relay sockets. Relay enters Linux `splice(2)` forwarding.
9. Travel and Home complete a separate mutual TLS handshake through both opaque forwarders.
10. Travel verifies the selected Home's exact URI identity and the business SPKI key set taken from the verified deployment trust. Only inside that business TLS connection does it name and open a service.

The selected credential ID is carried through route authorization and `OPEN_WORK`, then Home requires the business certificate to resolve to that exact grant and the encrypted `OPEN` to match its scope. A management certificate from one Travel identity therefore cannot be paired with another identity's business certificate, and a Home-scoped or Service-scoped grant cannot be widened by choosing a different mapping.

## Home-issued Travel credentials and live revocation

Travel generates two distinct P-256 private keys locally and immediately stores them as password-encrypted PKCS#8. Its enrollment request exposes only management/business CSRs with proof of possession and the requested stable Travel ID.

Each issuing Home has a password-encrypted management CA key, business CA key, and normal Home authority key. A designated Home may additionally hold a separate global authority key. The offline deployment root certifies their public keys and allowed roles/scopes in the signed deployment trust. Through a loopback-only embedded UI/API, an operator selects a bounded validity (365 days by default), enters the key password, and approves one explicit scope:

- `Global`: every logical business;
- `Home { home_id }`: every service on exactly one Home;
- `Service { home_id, service_id, protocol }`: one exact logical business.

A normal Home authority is root-certified only for its own Home and cannot mint Global or another Home's grant. Home decrypts the signing keys only for the request, verifies the CA and authority material against the signed deployment trust, signs the two CSR public keys under separate CA roots, then signs one credential that atomically binds the deployment/trust, request ID, independent 256-bit nonce and full request hash, both SPKIs, both exact CA and leaf-certificate hashes, authority epoch, credential ID, scope, and validity. It returns one self-contained Enrollment Response, zeroizes the password, and publishes only the signed public grant to Server. Travel import first verifies the response trust against its one embedded deployment-root public key; only then does it accept the response-carried CA and authority keys and verify every atomic binding, the encrypted local keys, certificate chains, and grant.

Issuance is application-level idempotent and each enrollment request is single-use. Home keeps a mode-`0600`, atomically replaced local ledger beside its issuer keys, keyed by a canonical SHA-256 fingerprint of the enrollment request. The complete public enrollment response and its authority, exact scope, and requested validity are journaled before Server publication; the acknowledged generation is recorded afterward. Server independently persists every spent request fingerprint in the same atomic state object as credentials, revocations, and generation, so reuse through another Home or after revocation is rejected. An identical retry therefore re-publishes an interrupted pending result or returns the byte-identical completed result, while changed authorization parameters are rejected.

Server authenticates the publishing Home, requires it to own the root-certified authority, verifies the signature and scope, persists the add-only credential set, increments the generation, and broadcasts the snapshot over established control sessions. The same Travel TLS identity may accumulate several independently revocable grants. Server filters the signed Travel-visible Catalog by all active grants; Relay selects a matching grant for each route. Server and OpenWrt contain the deployment-root public key and signed trust document but no deployment-root, CA, or authority private key.

Revocation also originates from the owning Home. Server validates ownership, persists the credential ID and reason, then broadcasts a new monotonic generation. Relay and Home verify every signature and persist the highest generation plus all observed revoked IDs. A lower generation or missing prior revocation is rejected even after restart. Applying an update does not restart a component. A revoked or expired credential cannot open new management sessions, routes, or Carriers; matching Relay, Server, Home, and target state is terminated. A revoked credential can never be restored; replacement uses a new key pair, certificate pair, and credential ID.

Every outer frame and payload has a hard bound. The stateful frame decoder is safe to resume after cancellation and all pre-trust/setup reads and writes have deadlines. Catalog/Relay counts, pending work/routes, control/data connections, active Home/Travel flows, Carriers globally and per Flow, unacknowledged byte buffers, and authorization state all have explicit ceilings. Carrier tasks acquire process and per-Flow permits before admission, and byte permits are held until the logical data has been acknowledged or discarded.

## Logical business routing

A Travel listener connects one logical business identified by `(home_id, service_id, protocol)`. `service_id` is unique only within one Home; two Homes may intentionally publish the same service ID and target port while remaining different businesses. The mapping's Home ID is immutable for the lifetime of a Flow. Relay competition replaces only the path to that Home: a failed, unavailable, or misconfigured Home never causes fallback to another Home, even when that other Home publishes the same service ID.

The aggregate catalog is informational and supports UI readiness. Route authorization uses the mapping's explicit Home ID, then business TLS and `OPEN` enforce the same Home/service pair. A Home disconnect removes only its catalog and route availability; other Homes and their Flows remain independent.

## Logical TCP

TCP uses a stable Flow ID plus replaceable Carrier IDs. `OPEN` attaches a complete end-to-end business-TLS Carrier to an existing or new Flow. Offset-bearing `DATA`, cumulative `ACK`/`DUP`, and offset-bearing `FIN`/`FIN_ACK` frames preserve ordered delivery in both directions. Backpressure comes from bounded unacknowledged buffers, bounded frame payloads, and the underlying TCP/TLS write path. Half-close is preserved: receipt of a valid logical FIN shuts down only the target write half until the reverse direction also finishes.

For a new Flow or reselection, Travel concurrently opens a complete Carrier through every known Relay and sends the same race ID and acknowledged offset on each path. Home accepts the first valid arrival with `RACE_ACK`; later arrivals for that race receive `RACE_DUPLICATE` containing the winning Carrier ID. Travel keeps the winner, closes the losing candidates, retransmits unacknowledged frames, and uses that Carrier until failure or the next periodic race. During periodic reevaluation, the active Carrier participates alongside new candidates through the other Relays. The interval begins at 60 seconds by default and doubles to a configurable 15-minute cap only when the active Carrier wins again. A different winner or a race with no winner is unstable and resets the interval to 60 seconds.

Carrier EOF, reset, TLS/read/write failure, or heartbeat expiry immediately detaches only the Carrier. Travel starts a new full-path race and locally backs off only when no candidate succeeds. Home never initiates a Carrier, but it retains the target TCP socket, offsets, and bounded reverse-direction retransmission data while detached. Home's detach timeout must exceed Travel's recovery timeout so ordinary Relay handover does not close the business TCP endpoints.

## Logical UDP

Each local client tuple becomes one association with one connected Home UDP socket. Datagram boundaries remain intact. Each direction has a monotonically increasing sequence, duplicates are discarded, and an idle timer reclaims the association. Per-association ingress queues are bounded and non-blocking: saturation drops only the current datagram instead of stalling the shared listener. UDP remains best effort and currently selects one usable Relay without Carrier migration; the protocol does not turn it into a reliable stream.

## Web UI

Travel Agent mounts `/api/status`, `/api/catalog`, and `/api/relays` before its embedded SPA fallback. The catalog contains only businesses authorized by the Travel identity's active grants, and each mapping is resolved with the composite Home/service/protocol key rather than service ID alone. Status includes the Relay-directory generation and Relays carrying active TCP Flows.

An issuing Home serves its issuer/revocation/key-maintenance SPA on a separate listener; a non-issuing Home omits `[issuer]` and carries no CA or authority private key. Both Home and Travel production UIs require an explicit `127.0.0.1` listener. Requests additionally require the exact listener authority in `Host`; modifying requests require the exact HTTP `Origin`, and cross-site Fetch Metadata is rejected. There is no production remote-listen escape hatch; remote administration uses an external authenticated tunnel. API routes expose issuer status, current signed grants, issue, revoke, and transactional issuer-key password rotation. Travel's local SPA similarly exposes transactional rotation for its management and business private keys. Rotation first verifies every key with the current password, stages and verifies every new encrypted PKCS#8 file, then replaces each path by same-directory atomic rename. A password-free file-name/hash journal completes an interrupted multi-file switch on the next start. Passwords are zeroized by the backend and are never persisted or written to a system keychain. Both SPAs emit identity, gzip, and Brotli representations. `embedded-spa` selects by `Accept-Encoding`, supplies strong representation-specific ETags, and prevents missing API or hashed-asset requests from receiving `index.html`.

## Portability

Linux Relay builds use `tokio-splice` for the zero-copy steady state. macOS retains the identical opaque-forwarding and protocol behavior through a portable copying fallback. Linux release targets use musl and are static; the macOS arm64 release is one application executable per component.

On OpenWrt, one package installs the Server and Relay executables, a UCI-to-TOML renderer, one procd service, and one LuCI page with standard Chinese/English catalogs. Server exposes one LAN-only Home control listener, a list of data listeners, a UCI list of independently pinned Home identities, and paths for the deployment-root public key, signed deployment trust, and Server control-signing key. Authority records come from the signed trust and are not duplicated in UCI. procd runs one named Server instance and one named process per enabled Relay section. Separate LAN and WAN6 Relay identities therefore share binaries and administration without sharing listener configuration or lifecycle state. LuCI configures and controls Server/Relay only; issuance and revocation remain on Home. The package is inert by default and leaves firewall policy outside its authority.

## Process restart boundary

No test or design claim may describe Home process restart as preserving an established TCP connection. The selected Home owns the target TCP socket, so restarting it destroys that socket; another Home is a different logical business and is not a substitute. Relay handover is a different requirement: the selected Home and Travel remain alive while only the replaceable Carrier changes.

Restarting Travel also creates a new process-session UUID and destroys its local client sockets. The new process may have to wait up to the 45-second old-session lease before Server admits it. Restarting Server clears the in-memory Travel lease registry but preserves the durable credential and revocation stores. After Server recovery the first process to reclaim each still-authorized Travel ID wins; duplicate-session exclusion is supplementary and is not a substitute for live revocation of stolen certificates and keys.
