#!/usr/bin/env python3
"""Reject numeric bootstrap hosts in distributable Travel packages."""

from __future__ import annotations

import argparse
import ipaddress
from pathlib import Path
import sys
import tomllib


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


def validate_travel_bootstrap(path: Path) -> None:
    with path.open("rb") as source:
        document = tomllib.load(source)
    relays = document.get("bootstrap_relays")
    if not isinstance(relays, list) or not relays:
        raise ValueError("bootstrap_relays must be a non-empty array")
    for entry_number, endpoint in enumerate(relays, start=1):
        if not isinstance(endpoint, str):
            raise ValueError(f"bootstrap_relays entry {entry_number} must be a string")
        host = endpoint_host(endpoint, entry_number)
        try:
            ipaddress.ip_address(host)
        except ValueError:
            continue
        raise ValueError(
            "distributable Travel packages must use DNS names, not IP literals, "
            f"in bootstrap_relays entry {entry_number}"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("travel_bootstrap", type=Path)
    args = parser.parse_args()
    try:
        validate_travel_bootstrap(args.travel_bootstrap)
    except (OSError, tomllib.TOMLDecodeError, ValueError) as error:
        print(f"Travel package privacy check failed: {error}", file=sys.stderr)
        return 1
    print("Travel package privacy check passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
