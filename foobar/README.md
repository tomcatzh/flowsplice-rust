# flowsplice-foobar

`flowsplice-foobar` is a low-rate continuity target and command-line probe for deployment acceptance.
It uses one ordinary TCP connection, never reconnects, and fails on timeout, EOF/reset, corruption,
duplication, or out-of-order echo data.

Run the Home-side loopback target:

```sh
flowsplice-foobar serve --listen 127.0.0.1:18080
```

Publish that address as a Home Agent TCP service, map the service through Travel Agent to
`127.0.0.1:10080`, then run:

```sh
flowsplice-foobar probe --addr 127.0.0.1:10080
```

The probe sends one 64-byte record immediately and then one record five seconds after each
successful response. The response timeout is 30 seconds. It prints machine-friendly `key=value`
events and runs until Ctrl-C without reconnecting. A bounded smoke test can use `--count`:

```sh
flowsplice-foobar probe --addr 127.0.0.1:10080 --count 12
```

`--interval-secs` and `--timeout-secs` override the low-rate defaults. A non-loopback server bind is
rejected unless `--allow-remote-listen` is explicit.
