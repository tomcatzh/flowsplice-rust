# Minimal business Home and Travel client

This is an independent Rust application with exactly two direct FlowSplice SDK dependencies: `flowsplice-home-core` and `flowsplice-travel-core`, fetched from `main`. Copy this directory into your own project location. It has a standalone workspace and a lockfile that pins its tested Git revision.

The protocol is one exchange: Travel sends `foobar`, Home replies `FOOBAR`, then Travel verifies the response, sends `ACK` and closes its write half. Home reads the acknowledgement and EOF, then closes its own write half. Travel reads EOF. Both sides wait for transport completion before shutting down their runtimes. Home checks the peer authorization lifetime. Incorrect bytes, timeouts and retained active flows fail with a nonzero exit code.

## Prepare

Use a disposable deployment with a running Server and Relay, and an administrator-provisioned business Home.

Follow [business Home provisioning](../../docs/business-home.md) to create the installation
and descriptor before running this sample. Obtain:

- The Home's `home-runtime.toml`, identity files, deployment trust and service grant.
- An approved TCP service ID, such as `foobar`, and its matching business descriptor JSON.
- A trusted deployment root public-key file, a Travel private-key password file and an independent installation directory.

Keep these materials outside the source directory. Home configuration file paths must be absolute. Use the Relay's reachable management IP address and port. The example trims whitespace from the hexadecimal root key; password files may have a trailing newline.

## Run

Build and enroll Travel first. An administrator must compare the displayed verification code and approve the requested business service. This example waits up to 110 seconds for approval.

```sh
cargo build --locked
cargo run --locked -- enroll RELAY_IP:PORT /absolute/path/descriptor.json /absolute/path/root.pub /absolute/path/travel-install /absolute/path/password.txt TRAVEL_ID
```

Retain the installation and enroll only once. Start Home in one terminal, then Travel in another:

```sh
cargo run --locked -- home /absolute/path/home-runtime.toml foobar
cargo run --locked -- travel /absolute/path/travel-install/travelagent.toml /absolute/path/password.txt /absolute/path/root.pub /absolute/path/descriptor.json
```

The Home service ID must match its configuration and descriptor. The exchange deadline is 120 seconds. Successful processes print `PASS Home` and `PASS Travel` and exit. Home waits for `peer.lifetime.ended()`. This client has only one connection, so it waits for `active_flows == 0` before shutting down.

The sample covers an exact TCP business. A persistent application must manage its own accept loop, concurrency, deadlines and authorization-end events. UDP and service-class discovery are separate integration paths.
