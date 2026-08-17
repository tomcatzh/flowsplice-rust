# Architecture

## Security boundary

FlowSplice has two independent trust domains:

1. Management TLS authenticates Server, Relay, Home, and Travel control links.
2. Business TLS authenticates only Travel and Home and carries logical TCP/UDP frames.

The CAs are intentionally separate. A Relay management key cannot authenticate as Travel to Home. In addition to certification-path and EKU verification, applications require the certificate URI SAN role and stable ID to match the protocol peer. SHA-256 SPKI allowlists further narrow every explicitly configured Server, Relay, Travel, management-Home, and business-Home peer relationship. All TLS configurations explicitly permit TLS 1.3 only and use rustls with the AWS-LC provider.

## Control topology

```text
                                   ┌--outbound mTLS--> Relay A <--mTLS--┐
Home Agent --outbound mTLS--> Server                                  Travel Agent
                                   └--outbound mTLS--> Relay B <--mTLS--┘
```

Home publishes the canonical service catalog to Server. Server maintains an isolated reconnect loop and sender for every configured Relay, pushes the catalog and complete Relay directory to each one, and each Relay fans changes out over authenticated Travel management sessions. A Travel Agent needs one reachable configured seed; after bootstrap it uses the received directory for management reconnection and Carrier competition. Server and Relay keep control traffic separate from data sockets. Control setup frames have deadlines, and established links are reclaimed after three missed heartbeat intervals.

## Route and data setup

1. An authenticated Travel control connection asks Relay for an opaque route. It does not reveal a service ID.
2. Relay asks Server for work on behalf of the certificate-bound Travel identity.
3. Server creates a random 32-byte work secret, records a short expiry, and asks Home to connect a work socket.
4. Relay creates a separate random 32-byte single-use route secret for Travel.
5. Travel authenticates its Relay data preface with HMAC-SHA256.
6. Relay atomically consumes the route and authenticates its work connection to Server with the Server-issued secret.
7. Home independently authenticates its work connection with the same secret.
8. Server pairs the Home and Relay sockets. Relay enters Linux `splice(2)` forwarding.
9. Travel and Home complete a separate mutual TLS handshake through both opaque forwarders.
10. Only inside that business TLS connection does Travel name and open a service.

Every outer frame and payload has a hard bound. The stateful frame decoder is safe to resume after cancellation and all pre-trust/setup reads have deadlines. Pending work, pending routes, and active Home/Travel flows have configurable process-local ceilings.

## Logical TCP

TCP uses a stable Flow ID plus replaceable Carrier IDs. `OPEN` attaches a complete end-to-end business-TLS Carrier to an existing or new Flow. Offset-bearing `DATA`, cumulative `ACK`/`DUP`, and offset-bearing `FIN`/`FIN_ACK` frames preserve ordered delivery in both directions. Backpressure comes from bounded unacknowledged buffers, bounded frame payloads, and the underlying TCP/TLS write path. Half-close is preserved: receipt of a valid logical FIN shuts down only the target write half until the reverse direction also finishes.

For a new Flow or reselection, Travel concurrently opens a complete Carrier through every known Relay and sends the same race ID and acknowledged offset on each path. Home accepts the first valid arrival with `RACE_ACK`; later arrivals for that race receive `RACE_DUPLICATE` containing the winning Carrier ID. Travel keeps the winner, closes the losing candidates, retransmits unacknowledged frames, and uses that Carrier until failure or the next periodic race. During periodic reevaluation, the active Carrier participates alongside new candidates through the other Relays. The interval begins at 60 seconds by default and doubles to a configurable 15-minute cap only when the active Carrier wins again. A different winner or a race with no winner is unstable and resets the interval to 60 seconds.

Carrier EOF, reset, TLS/read/write failure, or heartbeat expiry immediately detaches only the Carrier. Travel starts a new full-path race and locally backs off only when no candidate succeeds. Home never initiates a Carrier, but it retains the target TCP socket, offsets, and bounded reverse-direction retransmission data while detached. Home's detach timeout must exceed Travel's recovery timeout so ordinary Relay handover does not close the business TCP endpoints.

## Logical UDP

Each local client tuple becomes one association with one connected Home UDP socket. Datagram boundaries remain intact. Each direction has a monotonically increasing sequence, duplicates are discarded, and an idle timer reclaims the association. Per-association ingress queues are bounded and non-blocking: saturation drops only the current datagram instead of stalling the shared listener. UDP remains best effort and currently selects one usable Relay without Carrier migration; the protocol does not turn it into a reliable stream.

## Web UI

The Travel Agent mounts `/api/status`, `/api/catalog`, and `/api/relays` before the embedded SPA fallback. Status includes the Relay-directory generation and Relays carrying active TCP Flows. The build emits identity, gzip, and Brotli representations. `embedded-spa` selects by `Accept-Encoding`, supplies strong representation-specific ETags, and prevents missing API or hashed-asset requests from receiving `index.html`.

## Portability

Linux Relay builds use `tokio-splice` for the zero-copy steady state. macOS retains the identical opaque-forwarding and protocol behavior through a portable copying fallback. Linux release targets use musl and are static; the macOS arm64 release is one application executable per component.

## Process restart boundary

No test or design claim may describe Home process restart as preserving an established TCP connection. Home owns the target TCP socket, so restarting Home destroys that socket. Relay handover is a different requirement: Home and Travel remain alive while only the replaceable Carrier changes.
