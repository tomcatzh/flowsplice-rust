# OpenWrt integration

This directory contains the generic, non-secret OpenWrt capability shipped in the `flowsplice-openwrt` IPK:

- one UCI configuration covering the Server, its trusted Home Agent identities, multiple local Relay instances, and the published Relay directory;
- one procd service with named `server` and `relay_<section>` instances;
- one Chinese/English LuCI page for configuration, validation, lifecycle controls, and instance status;
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

After installation, place identity material below `/etc/flowsplice/pki` or configure other paths, restrict it to the `flowsplice` service account, then use LuCI or UCI to populate real values. Server has one LAN-only Home control listener shared by every configured Home identity; each `home` section contains one stable Home ID and its management SPKI pin list. The generated TOML uses one `[[homes]]` table per trusted identity, and at least one Home section is required. Its data listener list remains independently configurable. Each Relay section represents one explicit identity/listener, so LAN and WAN6 use separate sections and processes.

Travel access is distributed as authority-signed, scoped credentials. Issuance and revocation are performed by a Home Agent through its separate loopback-only embedded UI/API; LuCI and Server have no signing or revocation action. OpenWrt stores only trusted Home/global authority records and their public keys. Management/business CA private keys and authority private keys belong only on explicitly configured Home issuers and are never packaged in the IPK.

The Server keeps an add-only authorization set and revocation log, and each Relay keeps a persistent anti-rollback cache below `/etc/flowsplice/state`, allowing Home-published issuance or revocation to take effect without restarting any process. Validate before starting:

```sh
/etc/init.d/flowsplice validate
```

WAN firewall exposure is intentionally a separate operation.
