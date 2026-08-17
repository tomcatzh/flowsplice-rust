# OpenWrt integration

This directory contains the generic, non-secret OpenWrt capability shipped in the `flowsplice-openwrt` IPK:

- one UCI configuration covering the Server, multiple local Relay instances, and the published Relay directory;
- one procd service with named `server` and `relay_<section>` instances;
- one LuCI page for configuration, validation, lifecycle controls, and instance status;
- a renderer that produces strict per-process TOML below `/var/run/flowsplice`.

All installed instances are disabled by default. Package installation enables the inert init script but does not start a process or create firewall rules. Production addresses, certificates, keys, pins, UCI values, firewall rules, deployment scripts, and regression evidence must remain outside Git.

Build a target-matched IPK from already-built static Linux binaries:

```sh
python3 scripts/build-openwrt-ipk.py \
  --server dist/linux-arm64/flowsplice-server \
  --relay dist/linux-arm64/flowsplice-relay \
  --architecture aarch64_generic \
  --version 0.1.0 \
  --output-dir dist/openwrt
```

The builder emits the opkg-compatible gzip-compressed GNU-tar outer format and deterministic nested archives. Always confirm the target release, package manager, package architecture, ABI, and available storage before building. Validate the resulting archive locally, run the target package manager's no-action parser, and preserve a rollback snapshot before installation.

After installation, place identity material below `/etc/flowsplice/pki` or configure other paths, restrict it to the `flowsplice` service account, then use LuCI or UCI to populate real values. Travel access is distributed as offline-signed credentials; only the P-256 public key is installed on OpenWrt. The Server keeps an add-only revocation log and each Relay keeps a persistent anti-rollback cache below `/etc/flowsplice/state`, allowing revocation to take effect without restarting any process. Validate before starting:

```sh
/etc/init.d/flowsplice validate
```

WAN firewall exposure is intentionally a separate operation.
