# flowsplice-travel-core

Embed a Travel client in your own business application. Enroll a device, discover
authorized services and use in-process asynchronous TCP/UDP connections. Your
application owns the business protocol, user interface and platform integration.

```toml
[dependencies]
flowsplice-travel-core = { git = "https://github.com/tomcatzh/flowsplice-rust", branch = "main" }
anyhow = "1"
```

Default features are empty: ordinary SDK builds need no frontend assets or JavaScript
build. The optional `frontend` feature belongs to the repository's generic Travel
application, which enables it explicitly and builds `travelagent/web/dist` first.

This is a Git dependency. Cargo fetches the repository and builds the selected dependency
graph; commit your application's lockfile to pin the revision. Internal implementation
packages are transitive dependencies, not additional SDK entry points.

Provision a business Home using the
[setup guide](https://github.com/tomcatzh/flowsplice-rust/blob/main/docs/business-home.md).
For an exact target, pass its private `BusinessDescriptor` to `business::enroll` with
`BusinessEnrollmentOptions`. Enrollment needs a reachable Relay management address,
the independently trusted deployment root public key, a private-key password and
administrator approval. Preserve the installed identity directory between runs.

After enrollment, use the same descriptor to start a client and connect:

```no_run
use flowsplice_travel_core::{BusinessDescriptor, SocketStream, TravelCore};
use std::{future::Future, path::Path};

async fn connect_business<F, Fut>(
    config: &Path,
    password: &str,
    trusted_root_hex: &str,
    descriptor: &BusinessDescriptor,
    exchange: F,
) -> anyhow::Result<()>
where
    F: FnOnce(SocketStream) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let (travel, approved) = TravelCore::start_business(
        config, password, trusted_root_hex.trim(), descriptor,
    ).await?;
    let result = async {
        let stream = travel.connect_tcp(&approved.binding).await?;
        exchange(stream).await
    }.await;
    travel.shutdown().await;
    result
}
```

The exchange must complete its application acknowledgements and stream closure before
returning if it requires graceful transport completion. `SocketStream` implements Tokio
`AsyncRead` and `AsyncWrite`; `connect_udp` returns `SocketDatagrams` with `send` / `recv`.
One runtime can serve multiple views and connections. Close individual sockets when a
view ends; shut down the shared runtime when its application owner stops.

For discovery across matching Homes, use `ServiceClassDescriptor`, `service_class::enroll`,
`TravelCore::start_service_class` and `service_class_targets`. The selected Global authority
must approve that service class. `Catalog`, `HomeCatalog`, `RelayDirectory`, `RelayEndpoint`,
`DeploymentTrust`, signed grants and endpoint credential types are available from this
crate for typed application code.

See the [SDK guide](https://github.com/tomcatzh/flowsplice-rust/blob/main/crates/README.md),
[socket semantics](https://github.com/tomcatzh/flowsplice-rust/blob/main/docs/socket-runtime.md)
and [standalone foobar sample](https://github.com/tomcatzh/flowsplice-rust/tree/main/examples/sdk-foobar).
