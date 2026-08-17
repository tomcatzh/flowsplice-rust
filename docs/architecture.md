# Architecture

## Security boundary

FlowSplice has two independent trust domains:

1. Management TLS authenticates Server, Relay, Home, and Travel control links.
2. Business TLS authenticates only Travel and Home and carries logical TCP/UDP frames.

The CAs are intentionally separate. A Relay management key cannot authenticate as Travel to Home. In addition to certification-path and EKU verification, applications require the certificate URI SAN role and stable ID to match the protocol peer. SHA-256 SPKI allowlists further narrow every explicitly configured Server, Relay, Travel, management-Home, and business-Home peer relationship. All TLS configurations explicitly permit TLS 1.3 only and use rustls with the AWS-LC provider.

## Control topology

```text
Home Agent --outbound mTLS--> Server --outbound mTLS--> Relay <--mTLS-- Travel Agent
```

Home publishes the canonical service catalog to Server. Server pushes each generation to Relay, and Relay fans changes out over long-lived authenticated Travel catalog subscriptions. Server and Relay keep control traffic separate from data sockets. Control setup frames have deadlines, and established links are reclaimed after three missed heartbeat intervals.

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

TCP uses explicit `OPEN`, offset-bearing `DATA`, cumulative `ACK`, and offset-bearing `FIN` frames. Both directions validate contiguous offsets. Backpressure comes from bounded frame payloads and the underlying TCP/TLS write path. Half-close is preserved: receipt of a valid logical FIN shuts down only the target write half until the reverse direction also finishes.

The current executable does not retain acknowledged send state or detach a Flow from its Carrier. Each Flow owns one business-TLS/TCP Carrier, so Carrier loss closes that Flow. The required architecture instead keeps a stable in-memory Travel-to-Home Agent Session while Home and Travel remain alive, races complete end-to-end Carriers through the complete Server-authorized Relay directory, retains a primary plus warm standby, and reattaches existing Flows with bounded retransmission and deduplication when a Relay path fails or degrades.

## Logical UDP

Each local client tuple becomes one association with one connected Home UDP socket. Datagram boundaries remain intact. Each direction has a monotonically increasing sequence, duplicates are discarded, and an idle timer reclaims the association. Per-association ingress queues are bounded and non-blocking: saturation drops only the current datagram instead of stalling the shared listener. UDP remains best effort; the protocol does not turn it into a reliable stream.

## Web UI

The Travel Agent mounts `/api/status` and `/api/catalog` before the embedded SPA fallback. The build emits identity, gzip, and Brotli representations. `embedded-spa` selects by `Accept-Encoding`, supplies strong representation-specific ETags, and prevents missing API or hashed-asset requests from receiving `index.html`.

## Portability

Linux Relay builds use `tokio-splice` for the zero-copy steady state. macOS retains the identical opaque-forwarding and protocol behavior through a portable copying fallback. Linux release targets use musl and are static; the macOS arm64 release is one application executable per component.

## Process restart boundary

No test or design claim may describe Home process restart as preserving an established TCP connection. Home owns the target TCP socket, so restarting Home destroys that socket. Relay handover is a different requirement: Home and Travel remain alive while only the replaceable Carrier changes.
