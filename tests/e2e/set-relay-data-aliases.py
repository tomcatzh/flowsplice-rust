#!/usr/bin/env python3
"""Replace a Travel's Relay address overrides with operator aliases sharing one data address."""
import argparse
from pathlib import Path
import re
import tomllib

parser = argparse.ArgumentParser()
parser.add_argument("config", type=Path)
parser.add_argument("--data-addr", required=True)
parser.add_argument("--relay", action="append", required=True, metavar="ID=MANAGEMENT_ADDR")
args = parser.parse_args()

original = args.config.read_text()
aliases = []
for relay in args.relay:
    relay_id, management_addr = relay.split("=", 1)
    aliases.append({"id": relay_id, "management_addr": management_addr, "data_addr": args.data_addr})
text = re.sub(r"^\[\[relay_address_overrides\]\]\n(?:(?!\[)[^\n]*\n?)*", "", original, flags=re.M)
for alias in aliases:
    text += "\n[[relay_address_overrides]]\n" + "".join(f'{key} = "{value}"\n' for key, value in alias.items())

# Operator aliases (no bootstrap hint) are tried first; everything else must stay as it was.
before, after = tomllib.loads(original), tomllib.loads(text)
if after.pop("relay_address_overrides", None) != aliases:
    raise SystemExit("Relay aliases were not written as requested")
before.pop("relay_address_overrides", None)
if after != before:
    raise SystemExit("Rewriting Relay aliases changed other Travel settings")
args.config.write_text(text)
