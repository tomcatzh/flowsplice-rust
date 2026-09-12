# flowsplice-home-core

Embed a serving Home in your own business application. The runtime handles authenticated
TCP/UDP flows, authorization updates and connection recovery. Your application implements
the service protocol and owns its connection handlers.

```toml
[dependencies]
flowsplice-home-core = { git = "https://github.com/tomcatzh/flowsplice-rust", branch = "main" }
anyhow = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-util"] }
```

This is a Git dependency. Cargo fetches the repository and builds the selected dependency
graph; commit your application's lockfile to pin the revision. Internal implementation
packages are transitive dependencies, not additional SDK entry points.

First provision a business Home with `flowsplice-homeagent init --business-services`.
Follow the [business Home setup guide](https://github.com/tomcatzh/flowsplice-rust/blob/main/docs/business-home.md)
to obtain `home-runtime.toml`, credentials and a signed service grant. Deserialize that
file into `HomeRuntimeConfig`; the full administrative `homeagent.toml` has extra fields
and cannot be used as this configuration. The generated paths are absolute.

This example starts the serving task and stops it when the application requests shutdown:

```no_run
use flowsplice_home_core::{HomeRuntime, HomeRuntimeConfig, SocketServices};
use std::{future::Future, sync::Arc};

async fn run_home(config: HomeRuntimeConfig, stop: impl Future<Output = ()>) -> anyhow::Result<()> {
    let services = Arc::new(SocketServices::default());
    // Bind TCP/UDP listeners on `services` before loading the runtime.
    // Drive their accept loops concurrently in your application.
    let home = Arc::new(HomeRuntime::load(config, services)?);
    let running = Arc::clone(&home);
    let mut task = tokio::spawn(async move { running.run_serving().await });
    tokio::select! {
        result = &mut task => {
            home.shutdown().await;
            return result?;
        }
        () = stop => {}
    }
    home.shutdown().await;
    task.await?
}
```

Use `SocketServices::bind_tcp` / `bind_udp`, or implement `ServiceProvider`. Accept returns
I/O plus a `ServicePeer`. Observe its authorization lifetime while handling business
requests. `HomeServiceGrant`, `SignedHomeEndpointCredential`, `DeploymentTrust`,
`TravelCredentialScope`, `VerifiedAuthorization` and statistics query types are available
from this crate, so business code does not need direct internal package dependencies.

The runtime loads existing identity material. It does not run an enrollment issuer,
install a logger or start a Web UI. `shutdown()` cancels active I/O; finish application
acknowledgements and stream closure before shutting down when graceful completion matters.

See the [SDK guide](https://github.com/tomcatzh/flowsplice-rust/blob/main/crates/README.md),
[socket semantics](https://github.com/tomcatzh/flowsplice-rust/blob/main/docs/socket-runtime.md)
and [standalone foobar sample](https://github.com/tomcatzh/flowsplice-rust/tree/main/examples/sdk-foobar)
for the complete Home/Travel exchange.
