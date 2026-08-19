# Architecture

## Security boundary

FlowSplice has two independent trust domains:

1. Management TLS authenticates Server, Relay, Home, and Travel control links.
2. Business TLS authenticates only Travel and Home and carries logical TCP/UDP frames.

The CAs are intentionally separate. A Relay management key cannot authenticate as Travel to Home. In addition to certification-path and EKU verification, applications require the certificate URI SAN role and stable ID to match the protocol peer. SHA-256 SPKI allowlists narrow Server, Relay, management-Home, and business-Home relationships. Travel management and business SPKIs are jointly bound to one or more scoped credential grants by trusted Home/global ECDSA P-256 authority signatures. All TLS configurations explicitly permit TLS 1.3 only and use rustls with the AWS-LC provider.

## Control topology

```text
Home Agent A --outbound mTLS--┐       ┌--outbound mTLS--> Relay A <--mTLS--┐
                              ├--> Server                                  Travel Agent
Home Agent B --outbound mTLS--┘       └--outbound mTLS--> Relay B <--mTLS--┘
```

Each Home publishes its own catalog. Server authenticates each configured Home ID against that Home's management SPKI pins, keeps one replaceable session per Home ID, and publishes a sorted aggregate catalog. Reconnecting one Home supersedes only that ID's old session; disconnecting it removes only its catalog. Server also maintains an isolated reconnect loop and sender for every configured Relay, pushes the aggregate catalog, complete Relay directory, and Travel authorization snapshot to each one, and each Relay fans catalog/directory changes out over authenticated Travel management sessions. Every Home and Relay must durably apply and acknowledge the initial authorization generation before its control session becomes available. A Travel Agent needs one reachable configured seed; after bootstrap it uses the received directory for management reconnection and Carrier competition. Server and Relay keep control traffic separate from data sockets. Control setup frames have deadlines, and established links are reclaimed after three missed 10-second heartbeat intervals.

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
10. Travel verifies the selected Home's exact identity, server name, and SPKI pins. Only inside that business TLS connection does it name and open a service.

The selected credential ID is carried through route authorization and `OPEN_WORK`, then Home requires the business certificate to resolve to that exact grant and the encrypted `OPEN` to match its scope. A management certificate from one Travel identity therefore cannot be paired with another identity's business certificate, and a Home-scoped or Service-scoped grant cannot be widened by choosing a different mapping.

## Home-issued Travel credentials and live revocation

Travel generates two distinct P-256 private keys locally and immediately stores them as password-encrypted PKCS#8. Its enrollment request exposes only management/business CSRs with proof of possession and the requested stable Travel ID.

Each issuing Home has a password-encrypted management CA key, business CA key, and normal Home authority key. A designated Home may additionally hold a separate global authority key. Through a loopback-only embedded UI/API, an operator selects a bounded validity (365 days by default), enters the key password, and approves one explicit scope:

- `Global`: every logical business;
- `Home { home_id }`: every service on exactly one Home;
- `Service { home_id, service_id, protocol }`: one exact logical business.

A normal Home authority is cryptographically trusted only for its configured Home and cannot mint Global or another Home's grant. Home decrypts the signing keys only for the request, signs the two CSR public keys under separate CA roots, signs the scoped authorization payload, returns the enrollment response, zeroizes the password, and publishes only the signed public grant to Server. Travel import verifies the response against its original request, encrypted local keys, both CA roots, authority public key, certificates, and signed grant before installing it.

Server authenticates the publishing Home, requires it to own the configured authority, verifies the signature and scope, persists the add-only credential set, increments the generation, and broadcasts the snapshot over established control sessions. The same Travel TLS identity may accumulate several independently revocable grants. Relay filters the catalog by all active grants and selects a matching grant for each route. Server and OpenWrt contain authority public keys but no signing private key.

Revocation also originates from the owning Home. Server validates ownership, persists the credential ID and reason, then broadcasts a new monotonic generation. Relay and Home verify every signature and persist the highest generation plus all observed revoked IDs. A lower generation or missing prior revocation is rejected even after restart. Applying an update does not restart a component. A revoked or expired credential cannot open new management sessions, routes, or Carriers; matching Relay, Server, Home, and target state is terminated. A revoked credential can never be restored; replacement uses a new key pair, certificate pair, and credential ID.

Every outer frame and payload has a hard bound. The stateful frame decoder is safe to resume after cancellation and all pre-trust/setup reads have deadlines. Pending work, pending routes, and active Home/Travel flows have configurable process-local ceilings.

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

Home Agent serves its issuer/revocation/key-maintenance SPA on a separate listener, loopback-only by default. API routes expose issuer status, current signed grants, issue, revoke, and transactional issuer-key password rotation. Travel's local SPA similarly exposes transactional rotation for its management and business private keys. Rotation first verifies every key with the current password, stages and verifies every new encrypted PKCS#8 file, then replaces each path by same-directory atomic rename. A password-free file-name/hash journal completes an interrupted multi-file switch on the next start. Passwords are zeroized by the backend and are never persisted or written to a system keychain. Rotation endpoints require a loopback listener and remain unavailable on a remotely bound HTTP UI. A non-loopback bind for the remaining UI/API requires both an explicit configuration gate and an administrator bearer token; signing still requires the private-key password on every request. Both SPAs emit identity, gzip, and Brotli representations. `embedded-spa` selects by `Accept-Encoding`, supplies strong representation-specific ETags, and prevents missing API or hashed-asset requests from receiving `index.html`.

## Portability

Linux Relay builds use `tokio-splice` for the zero-copy steady state. macOS retains the identical opaque-forwarding and protocol behavior through a portable copying fallback. Linux release targets use musl and are static; the macOS arm64 release is one application executable per component.

On OpenWrt, one package installs the Server and Relay executables, a UCI-to-TOML renderer, one procd service, and one LuCI page with standard Chinese/English catalogs. Server exposes one LAN-only Home control listener, a list of data listeners, a UCI list of independently pinned Home identities, and a trusted Home/global authority list. procd runs one named Server instance and one named process per enabled Relay section. Separate LAN and WAN6 Relay identities therefore share binaries and administration without sharing listener configuration or lifecycle state. LuCI configures and controls Server/Relay only; issuance and revocation remain on Home. The package is inert by default and leaves firewall policy outside its authority.

## Process restart boundary

No test or design claim may describe Home process restart as preserving an established TCP connection. The selected Home owns the target TCP socket, so restarting it destroys that socket; another Home is a different logical business and is not a substitute. Relay handover is a different requirement: the selected Home and Travel remain alive while only the replaceable Carrier changes.

Restarting Travel also creates a new process-session UUID and destroys its local client sockets. The new process may have to wait up to the 45-second old-session lease before Server admits it. Restarting Server clears the in-memory Travel lease registry but preserves the durable credential and revocation stores. After Server recovery the first process to reclaim each still-authorized Travel ID wins; duplicate-session exclusion is supplementary and is not a substitute for live revocation of stolen certificates and keys.
