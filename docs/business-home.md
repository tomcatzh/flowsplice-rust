# Provision a business Home

Use the existing FlowSplice Server, Relay and administrative Home issuer. The business
Home runs your application with `flowsplice-home-core`; the issuer approves enrollment
separately. The business Home receives serving credentials and a service grant, without
issuer private keys.

## Prepare trusted bootstrap inputs

Obtain the `flowsplice-homeagent` provisioning executable and a `home-bootstrap.toml`
from the deployment administrator. If building that executable from source, run
`make web` and `cargo build --locked -p flowsplice-homeagent` in the FlowSplice checkout.
Your business application still depends only on its SDK crate.

The bootstrap file requires all of these fields, using values from your deployment:

```toml
deployment_root_public_key = "/absolute/path/deployment-root.pub"
deployment_trust = "/absolute/path/deployment-trust.json"
server_id = "YOUR_SERVER_ID"
server_name = "YOUR_SERVER_NAME"
server_control_port = 7443
ui_listen = "127.0.0.1:18080"
```

Replace the example port and identities with the administrator's values. Obtain the
root public key through an independently trusted channel and use the matching signed
deployment trust. `ui_listen` is required by the bootstrap schema and must be loopback;
the serving-only result does not start a Web UI.

Validate the bootstrap input before enrollment:

```sh
flowsplice-homeagent check-bootstrap-config --config /absolute/path/home-bootstrap.toml
```

## Request the application's services

Create `services.json` with your application protocol and capabilities. A minimal TCP
service for the foobar sample is:

```json
[
  {
    "service_id": "foobar",
    "protocol": "tcp",
    "application_protocol": "example.foobar.v1",
    "capabilities": ["read", "write"]
  }
]
```

Choose a dedicated installation directory, then enroll against the Server's IP address:

```sh
flowsplice-homeagent init \
  --server SERVER_IP \
  --bootstrap-config /absolute/path/home-bootstrap.toml \
  --business-services /absolute/path/services.json \
  --install-dir /absolute/path/business-home
```

Keep this command running while an administrator reviews the pending Home enrollment
in the existing issuer's Home UI. Compare the displayed verification code and approve
the requested business services with the serving-only profile. Home approval uses
`POST /api/home-enrollment/approve`; Travel device approval later uses
`POST /api/enrollment/approve`. These are separate approval steps.

Successful installation creates:

- `home-runtime.toml`, containing the serving configuration with absolute paths.
- `cert/`, containing identity material, deployment trust, the endpoint credential and
  the signed service grant.
- `state/`, containing initialized authorization and runtime state.
- One private `business-<hex-service-id>.json` descriptor per approved service.
  For `foobar`, the filename is `business-666f6f626172.json`.

Keep private keys and state in the installation directory. Give the matching descriptor
to your business client through your application's distribution channel; do not publish
the installation directory. Preserve these materials across normal upgrades.

## Start the business application

Deserialize `home-runtime.toml` into `flowsplice_home_core::HomeRuntimeConfig` using
your application's TOML parser, register listeners for the approved service IDs, then
call `HomeRuntime::load` and drive `run_serving()` concurrently with the accept loops.
The generated catalog uses `target = "in-process"`; your `ServiceProvider` or
`SocketServices` implements the service.

Do not pass the administrative `homeagent.toml` to `HomeRuntimeConfig`: that schema
rejects unknown fields such as `ui_listen` and `issuer`. Provisioning does not start or
install your business binary; your application owns its process/service packaging.

For the Travel client, use the generated descriptor with `business::enroll`, the Relay's
management IP address and port, and the independently trusted root key. Approve that
client in the issuer identified by the descriptor. Start subsequent sessions with the
installed `travelagent.toml` and the same descriptor. A service-class client instead
requires a `ServiceClassDescriptor` and Global authority approval for the selected class.

Continue with the [SDK guide](../crates/README.md) or run the
[standalone foobar sample](../examples/sdk-foobar/README.md).
