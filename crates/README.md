# Build your own business with FlowSplice

FlowSplice provides two supported SDK entry points:

| Dependency | Purpose |
| --- | --- |
| [`flowsplice-home-core`](flowsplice-home-core/README.md) | Accept authorized connections in your business server |
| [`flowsplice-travel-core`](flowsplice-travel-core/README.md) | Enroll, discover and connect from your business client |

Your application owns its message format, request handling, application permissions and user interface.

## Install

Create an independent Rust project and select the dependency for each role. An application implementing both roles needs both entries:

```toml
[dependencies]
flowsplice-home-core = { git = "https://github.com/tomcatzh/flowsplice-rust", branch = "main" }
flowsplice-travel-core = { git = "https://github.com/tomcatzh/flowsplice-rust", branch = "main" }
```

These are Git dependencies. Travel's default features are empty, so an SDK consumer needs no Web assets or frontend build. The generic Travel application explicitly enables `frontend` and builds its assets separately. The snippets below also use `anyhow = "1"`, `serde_json = "1"` and Tokio 1 with `rt-multi-thread`, `macros` and `io-util` enabled.

Cargo fetches the repository and builds the selected dependency graph. Your application has its own workspace. The two SDK entry points have internal transitive dependencies. Commit your application's `Cargo.lock` to retain the resolved Git revision; update it deliberately when adopting a newer revision.

## Prerequisites

Use a running FlowSplice Server and Relay, and a business Home provisioned by an administrator. Home needs existing certificates, private keys, deployment trust, authorization state and service grants. `HomeRuntime::load` verifies and loads these materials; provisioning happens beforehand.

Follow [business Home provisioning](../docs/business-home.md) for the `flowsplice-homeagent init --business-services` command, approval and generated files. Load its `home-runtime.toml`; the administrative `homeagent.toml` contains fields the SDK configuration rejects.

Travel needs the matching business descriptor, a reachable Relay management IP address and port, and a deployment root public key obtained through an independently trusted channel. Enrollment requires administrator approval. Retain the installed identity between runs. Store configuration and private identity material in your application's data directory.

## Business Home

Import `HomeRuntimeConfig`, `Service`, `ServiceProtocol`, `SocketServices` and listener types from `flowsplice_home_core`. Configured service IDs and protocols must match the approved services. Use absolute file paths when loading the provisioned Home configuration directly.

Business API types are available through the two SDK entries: Home exports grants, endpoint credentials, trust, authorization and statistics query types; Travel exports descriptors, trust, grants, `Catalog`, `HomeCatalog`, `RelayDirectory` and `RelayEndpoint`. Do not add direct internal package dependencies to name these types.

This function accepts one TCP connection and demonstrates runtime ownership. The caller supplies the configuration and a handler responsible for message boundaries, timeouts and authorization lifetime:

```rust
use anyhow::Result;
use flowsplice_home_core::{BoxStream, HomeRuntime, HomeRuntimeConfig, ServicePeer, SocketServices};
use std::{future::Future, sync::Arc};

pub async fn serve_one<F, Fut>(
    config: HomeRuntimeConfig,
    service_id: String,
    handle: F,
) -> Result<()>
where
    F: FnOnce(BoxStream, ServicePeer) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let services = Arc::new(SocketServices::default());
    let mut listener = services.bind_tcp(service_id, 16)?;
    let runtime = Arc::new(HomeRuntime::load(config, services)?);
    let running = Arc::clone(&runtime);
    let mut task = tokio::spawn(async move { running.run_serving().await });
    let work = async {
        let (stream, peer) = listener.accept().await?;
        handle(stream, peer).await
    };
    let result = tokio::select! {
        finished = &mut task => {
            runtime.shutdown().await;
            return finished?;
        }
        result = work => result,
    };
    runtime.shutdown().await;
    task.await??;
    result
}
```

Keep `run_serving()` running concurrently with your accept loop. For a persistent server, manage connection tasks and an application stop signal. On exit, call `shutdown().await` and await the serving task.

Complete application acknowledgements and stream closure before stopping the runtime. `runtime.shutdown()` terminates active connections. The [standalone foobar example](../examples/sdk-foobar/README.md) shows a complete exchange and transport completion, including both half-close orders.

UDP services use `bind_udp`; `accept()` returns datagram I/O and authenticated peer metadata. `ServicePeer` includes identity and `lifetime`: check `is_active()` before business operations and observe `ended()` while waiting. Buffered input may remain after authorization ends.

## Business Travel client

Both enrollment modes use `business::BusinessEnrollmentOptions`: provide a client ID, installation directory, Relay management IP address and port, trusted root key contents, private-key password and approval timeout in seconds.

The `root` argument contains the deployment root public key as hexadecimal text. Read the file and call `.trim()` before passing its contents to enrollment or startup:

```rust
let root = std::fs::read_to_string(root_file)?;
let root = root.trim();
```

Select the enrollment mode matching the application's authorization:

- **Exact target:** parse a `BusinessDescriptor`, call `business::enroll`, then use the same descriptor with `TravelCore::start_business`. Connect using the returned `approved.binding`.
- **Service class:** parse a `ServiceClassDescriptor`, call `service_class::enroll`, then use `TravelCore::start_service_class`. Call `service_class_targets(&approved).await`, select a verified target in your application and construct its `ServiceBinding`. Discovery can initially return no targets.

The application supplies the arguments and an enrollment progress callback when it needs to display the verification code:

```rust
pub async fn enroll_exact(
    options: flowsplice_travel_core::business::BusinessEnrollmentOptions,
    descriptor_json: &str,
) -> anyhow::Result<()> {
    let descriptor: flowsplice_travel_core::BusinessDescriptor =
        serde_json::from_str(descriptor_json)?;
    flowsplice_travel_core::business::enroll(options, descriptor, |_| {}).await
}

pub async fn enroll_class(
    options: flowsplice_travel_core::business::BusinessEnrollmentOptions,
    descriptor_json: &str,
    label: String,
) -> anyhow::Result<()> {
    let descriptor: flowsplice_travel_core::ServiceClassDescriptor =
        serde_json::from_str(descriptor_json)?;
    flowsplice_travel_core::service_class::enroll(options, descriptor, label, |_| {}).await
}
```

After installation, an exact TCP business can open an in-process asynchronous stream:

```rust
pub async fn use_exact<F, Fut>(
    config: &std::path::Path,
    password: &str,
    root: &str,
    descriptor: &flowsplice_travel_core::BusinessDescriptor,
    handle: F,
) -> anyhow::Result<()>
where
    F: FnOnce(flowsplice_travel_core::SocketStream) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let (runtime, approved) = flowsplice_travel_core::TravelCore::start_business(
        config, password, root, descriptor,
    ).await?;
    let result = async {
        let stream = runtime.connect_tcp(&approved.binding).await?;
        handle(stream).await
    }.await;
    runtime.shutdown().await;
    result
}
```

The handler should complete its application exchange before returning. Applications that require graceful transport completion must keep the stream and runtime alive through that completion; see the foobar example. UDP services use `connect_udp`. Neither API requires a local client listening port or port mapping. A runtime can support multiple business connections.

These functions require deployment materials supplied by your application. Validate enrollment approval, actual request/response bytes and shutdown in a disposable deployment before integration.
