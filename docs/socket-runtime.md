# Embedded socket runtimes

The generic Home and Travel applications and embedded businesses use the same encrypted
transport implementations. Existing transport frames, certificates and legacy authorization
shapes are retained; service-category authorization adds an explicit new scope and signed catalog metadata.

- `flowsplice-home-core` owns serving identity validation, Server control, authorization
  synchronization, business TLS, Carrier recovery and TCP/UDP flow handling.
- `flowsplice-travel-core` owns trusted discovery, device identity, encrypted connections
  and recovery. Set `default-features = false` for an embedded/native consumer. The
  `frontend` feature provides the existing CLI and embedded Web adapter.
- `flowsplice-transport` defines application I/O, verified peer context and bounded
  in-process Home listeners. It opens no physical sockets.
- The generic Home adapter connects physical target sockets. The generic Travel adapter
  retains configurable local forwarding listeners and background behavior.

## Travel

Start an active-use runtime with provisioned identity and an independently trusted root:

```rust,ignore
let travel = TravelCore::start_in_process(config_path, password, trusted_root).await?;
let stream = travel.connect_tcp(&ServiceBinding {
    home_id: approved_home,
    service_id: approved_service,
    protocol: ServiceProtocol::Tcp,
}).await?;
```

`SocketStream` implements Tokio `AsyncRead` and `AsyncWrite`, including partial I/O,
backpressure and write half-close. Connect returns after authenticated service admission
and Carrier selection. Dropping the stream cancels that Flow. It does not represent
deletion of any application resource behind the stream.

`connect_udp` returns `SocketDatagrams` with asynchronous `send` and `recv`. Messages,
including empty messages, retain their boundaries. Payloads larger than 65507 bytes fail.
Receive cancellation does not consume the next message. UDP does not promise reliable
delivery or replay across a network change; a closed association requires a new connect.

In-process startup ignores existing forwarding mappings and refuses mapping mutations.
It opens no local TCP, UDP, HTTP or WebSocket listener. Root/password validation still
applies. Multiple operations share this runtime and its device identity; do not start an
independent runtime per terminal tab. Call `shutdown().await` when the active business
view ends to drain connections, background work and persistent statistics. Dropping a
runtime also cancels its tasks, while explicit shutdown provides the drain boundary.

The destination binding contains no local bind address. Constructing one does not grant
access: Server/Relay/Home still enforce the signed device authorization and exact service.

## Service-category enrollment and discovery

Use `service_class::enroll` with `BusinessEnrollmentOptions`, a `ServiceClassDescriptor`
and the readable device label. It shares the resumable enrollment/journal engine with
exact business enrollment. `TravelCore::start_service_class` validates the installed
signed intent and returns `(TravelCore, ApprovedServiceClass)`; there is one device
identity and management connection for all matching Homes.

Call `service_class_targets(&approved)` to obtain verified current Home/service
bindings. The category is the exact application protocol and TCP/UDP transport,
independent of Home ID or service display name. Each target must have a valid signed
Home service grant and endpoint. New matching Homes appear without another client
enrollment or a packaged Home list. `connect_tcp` / `connect_udp` retain ordinary
socket semantics; disconnecting one socket does not shut down the shared runtime.

The new `ServiceClass` credential must be issued by the selected Global authority.
It grants only matching approved businesses. Upgrade all Server, Relay and Home
components consuming authorization snapshots before issuing it: older infrastructure
cannot parse this new scope. Existing generic Travel clients keep their old credentials
and message shapes. Legacy exact business installations are preserved and do not
silently acquire category-wide access. See [PTY provisioning](pty.md).

## Home

Create listeners before starting the provisioned serving runtime:

```rust,ignore
let services = Arc::new(SocketServices::default());
let mut listener = services.bind_tcp(service_id, 16)?;
let home = HomeRuntime::load(config, services)?;
// Drive home.run_serving() concurrently with application acceptance.
let (stream, peer) = listener.accept().await?;
```

The configured service catalog still supplies the ID, protocol and display metadata.
For a virtual service, `target` is nonempty opaque metadata such as `in-process`; it is
never opened as a socket by the runtime. Upgrade the deployment as one coordinated
current-version system; mixed-version support is not required. `peer` contains the verified
Travel credential and Flow identity; application
permissions can narrow that authorization further.

`bind_udp` accepts separate datagram endpoints with authenticated peer context. Replies
use that endpoint rather than an arbitrary destination address. Accept queues, stream
buffers and datagram queues are bounded. Dropping a listener removes its registration;
existing accepted I/O has its own lifetime.

`run_serving` loads no issuer keys and rejects enrollment requests. The existing Home
application uses a separate administrative adapter for issuance and enrollment. Library
startup does not install a logger, prompt for input, launch a Web server, issue certificates
or choose application storage directories.

For orderly shutdown, call `home.shutdown().await`, await the running `run_serving`
future, then drop the runtime. Shutdown cancels and joins outstanding handshakes,
flows, target readers and Carriers even when application I/O is blocked. Dropping the
runtime or cancelling its run future also signals cancellation. A stopped runtime is
terminal; load a new runtime to restart. Keep application session ownership separate
from transport lifetime.

## Acceptance

`tests/check-travel-review-regressions.sh` retains the original regression cases and
checks in-process identity validation, absence of legacy listeners, invalid bindings and
connection-cancellation cleanup. The full Docker runner additionally exercises encrypted
in-process TCP/UDP against both the old target adapter and an issuer-free embedded Home.
Set `FLOWSPLICE_E2E_TRAVEL_IMAGE` to a separately built released image to run old generic
Travel runtimes against the new backend; native clients use their separate platform suites.
