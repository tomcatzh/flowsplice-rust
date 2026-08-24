#!/usr/bin/env python3
"""Fail closed unless a public bootstrap sample is deployment-neutral."""

from __future__ import annotations

import argparse
import ipaddress
from pathlib import Path
import sys
import tomllib


EXAMPLE_DOMAINS = ("example.com", "example.net", "example.org")


def is_example_hostname(host: str) -> bool:
    normalized = host.rstrip(".").lower()
    return any(
        normalized == domain or normalized.endswith(f".{domain}")
        for domain in EXAMPLE_DOMAINS
    )


def endpoint_host(endpoint: str, entry_number: int) -> str:
    if endpoint.startswith("["):
        closing = endpoint.find("]")
        if closing < 0 or closing + 1 >= len(endpoint) or endpoint[closing + 1] != ":":
            raise ValueError(f"bootstrap_relays entry {entry_number} is invalid")
        host = endpoint[1:closing]
        port = endpoint[closing + 2 :]
    else:
        host, separator, port = endpoint.rpartition(":")
        if not separator:
            raise ValueError(f"bootstrap_relays entry {entry_number} has no port")
    if not host or not port.isdecimal() or not 1 <= int(port) <= 65535:
        raise ValueError(f"bootstrap_relays entry {entry_number} is invalid")
    return host


def validate_remote_hostname(host: str, field: str) -> None:
    try:
        ipaddress.ip_address(host)
    except ValueError:
        if is_example_hostname(host):
            return
        raise ValueError(
            f"{field} must use an IANA-reserved example hostname; "
            "real IP addresses and hostnames are secret-equivalent"
        ) from None
    raise ValueError(
        f"{field} must not contain an IP address; use an IANA-reserved example hostname"
    )


def validate_bootstrap_sample(path: Path) -> None:
    if not path.name.endswith(".example.toml"):
        raise ValueError("public bootstrap configuration must use the .example.toml suffix")
    with path.open("rb") as source:
        document = tomllib.load(source)
    relays = document.get("bootstrap_relays")
    server_name = document.get("server_name")
    if relays is not None:
        expected_keys = {
            "deployment_root_public_key",
            "deployment_trust",
            "bootstrap_relays",
            "ui_listen",
        }
        if set(document) != expected_keys:
            raise ValueError("Travel bootstrap sample keys do not match the public allowlist")
        if not isinstance(relays, list) or not relays:
            raise ValueError("bootstrap_relays must be a non-empty array")
        for entry_number, endpoint in enumerate(relays, start=1):
            if not isinstance(endpoint, str):
                raise ValueError(f"bootstrap_relays entry {entry_number} must be a string")
            validate_remote_hostname(
                endpoint_host(endpoint, entry_number),
                f"bootstrap_relays entry {entry_number}",
            )
    elif server_name is not None:
        expected_keys = {
            "deployment_root_public_key",
            "deployment_trust",
            "server_id",
            "server_name",
            "server_control_port",
            "ui_listen",
        }
        if set(document) != expected_keys:
            raise ValueError("Home bootstrap sample keys do not match the public allowlist")
        if not isinstance(server_name, str) or not server_name:
            raise ValueError("server_name must be a non-empty string")
        validate_remote_hostname(server_name, "server_name")
    else:
        raise ValueError("bootstrap sample has no supported remote endpoint field")
    if document.get("deployment_root_public_key") != "deployment-root.pub":
        raise ValueError("deployment root sample path must use the generic adjacent filename")
    if document.get("deployment_trust") != "deployment-trust.json":
        raise ValueError("deployment trust sample path must use the generic adjacent filename")
    ui_listen = document.get("ui_listen")
    if not isinstance(ui_listen, str):
        raise ValueError("ui_listen must be a string")
    try:
        ui_host = ipaddress.ip_address(endpoint_host(ui_listen, 1))
    except ValueError:
        raise ValueError("ui_listen must use an IP loopback address") from None
    if not ui_host.is_loopback:
        raise ValueError("ui_listen must use an IP loopback address")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("bootstrap_sample", type=Path)
    args = parser.parse_args()
    try:
        validate_bootstrap_sample(args.bootstrap_sample)
    except (OSError, tomllib.TOMLDecodeError, ValueError) as error:
        print(f"Public package privacy check failed: {error}", file=sys.stderr)
        return 1
    print("Public package privacy check passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
