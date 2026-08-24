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
  --version 0.2.1 \
  --output-dir dist/openwrt
```

The builder emits the opkg-compatible gzip-compressed GNU-tar outer format and deterministic nested archives. Always confirm the target release, package manager, package architecture, ABI, and available storage before building. Validate the resulting archive locally, run the target package manager's no-action parser, and preserve a rollback snapshot before installation.

After installation, place identity material below `/etc/flowsplice/pki` or configure other paths, restrict it to the `flowsplice` service account, then use LuCI or UCI to populate real values. Server has one LAN-only Home control/active-Relay listener and one loopback-only statistics listener; it has no business-data listener. Each `home` section contains only one stable Home ID. The verified deployment trust supplies that Home's management SPKI key set. The generated TOML uses one `[[homes]]` table per permitted identity, and at least one Home section is required. Each Server Relay entry declares `active` or `passive`; passive entries require a stable management seed while active entries contain no Relay address. Each Relay process has the matching connection mode, independent management/data listeners, one required OpenWrt logical network, redb state store, and loopback statistics listener. Exact listeners report their own address; wildcard IPv4/IPv6 listeners read the current address and `l3_device` from netifd/ubus. LAN, WAN IPv4, and WAN IPv6 therefore use separate sections and processes without any duplicated advertised-address option. Home connects its authorized business socket directly to the selected Relay's authenticated published data address.

Travel access is distributed as authority-signed, scoped credentials. Issuance and revocation are performed by a Home Agent through its separate loopback-only embedded UI/API; LuCI and Server have no signing or revocation action. OpenWrt stores the deployment-root public key, root-signed deployment trust, and the epoch-limited Server control-signing private key. The trust document binds both CA roots, Home endpoint SPKIs, and Home/global authority epochs, roles, and scopes. The deployment-root private key is never a runtime input; it remains offline and is never mounted into Home, Server, Relay, or packaged in the IPK. Management/business CA and authority private keys remain only on explicitly configured Home issuers.

Server signs each Travel-visible Relay-directory and filtered-Catalog snapshot as one short-lived payload. Every Relay has an explicit connection mode. A passive Relay is reached through one fixed Server-side management seed; an active Relay connects to Server and requires no Relay address in Server configuration. Both directions become the same authenticated bidirectional control session, over which Relay reports, updates, or withdraws its management/data endpoints. Server maintains one generation-numbered complete in-memory Relay directory and broadcasts the full replacement snapshot to every connected Relay after any endpoint or session change; Relay never assembles it from partial updates. Directory changes and signed Travel snapshots share the persisted control-generation high-water mark, preventing generation rollback after Server restart. OpenWrt LAN and WAN6 instances should use active mode and the fixed LAN Server control address. Relay only transports the signed directory, so a compromised Relay cannot rewrite another Relay's directory entry or Catalog metadata. Seed addresses remain reachability inputs rather than published truth or trust anchors.

Active Relay endpoint registration reuses the existing `serverAuth` endpoint identity. Server accepts that certificate purpose only for an explicitly configured active Relay role/ID. Passive mode continues to authenticate the same certificate as the Relay TLS server. Existing Relay leaves need no `clientAuth` EKU and no reissuance during upgrade.

The Server keeps one atomic authorization state containing the generation, add-only credentials, irreversible revocations, and spent enrollment-request hashes, plus a separate durable control-snapshot generation high-water mark. Server and every Relay also have separate redb state stores for five-minute business statistics and durable signed-report delivery. Each Relay keeps a persistent anti-rollback cache below `/etc/flowsplice/state`, allowing Home-published issuance or revocation to take effect without restarting any process. The package initializes the two empty Server JSON state files only when absent and never overwrites them on upgrade; redb creates its own database on first start. The init script assigns rendered configuration and state ownership to `flowsplice`, then runs `--check-config` as that exact service account before starting, so private-key ownership validation matches runtime. Validate before starting:

```sh
/etc/init.d/flowsplice validate
```

After validating a newly configured Relay and before its first start, explicitly initialize its anti-rollback state using that instance's rendered TOML, for example:

```sh
su -s /bin/sh -c '/usr/bin/flowsplice-relay --config /var/run/flowsplice/relay_lan.toml --initialize-authorization-state' flowsplice
```

Repeat once for every enabled Relay section. The command is idempotent but never replaces existing state. If a running installation later reports missing or invalid authorization state, restore the matching backup instead of initializing an empty cache.

WAN firewall exposure is intentionally a separate operation.
