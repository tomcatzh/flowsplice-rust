#!/usr/bin/env python3
"""Compile the subset of GNU PO used by LuCI into its LMO catalog format."""

from __future__ import annotations

import ast
from pathlib import Path
import struct
from typing import Iterable


def _quoted_value(line: str) -> str:
    start = line.find('"')
    if start < 0:
        raise ValueError(f"missing quoted PO value: {line!r}")
    value = ast.literal_eval(line[start:])
    if not isinstance(value, str):
        raise ValueError(f"invalid PO string: {line!r}")
    return value


def parse_po(path: Path) -> list[tuple[str, str]]:
    entries: list[tuple[str, str]] = []
    for block in path.read_text(encoding="utf-8").split("\n\n"):
        msgid = ""
        msgstr = ""
        current: str | None = None
        fuzzy = False
        for raw_line in block.splitlines():
            line = raw_line.strip()
            if line.startswith("#,") and "fuzzy" in line:
                fuzzy = True
            elif line.startswith("msgid "):
                msgid = _quoted_value(line)
                current = "msgid"
            elif line.startswith("msgstr "):
                msgstr = _quoted_value(line)
                current = "msgstr"
            elif line.startswith('"'):
                if current == "msgid":
                    msgid += _quoted_value(line)
                elif current == "msgstr":
                    msgstr += _quoted_value(line)
        if msgid and msgstr and not fuzzy:
            entries.append((msgid, msgstr))
    return entries


def super_fast_hash(text: str) -> int:
    data = text.encode("utf-8")
    remaining = len(data)
    value = remaining
    offset = 0
    while remaining >= 4:
        first = data[offset] | (data[offset + 1] << 8)
        second = data[offset + 2] | (data[offset + 3] << 8)
        value = (value + first) & 0xFFFFFFFF
        temporary = (second << 11) ^ value
        value = ((value << 16) ^ temporary) & 0xFFFFFFFF
        value = (value + (value >> 11)) & 0xFFFFFFFF
        offset += 4
        remaining -= 4
    if remaining == 3:
        value = (value + data[offset] + (data[offset + 1] << 8)) & 0xFFFFFFFF
        value ^= (value << 16) & 0xFFFFFFFF
        third = data[offset + 2] - 256 if data[offset + 2] >= 128 else data[offset + 2]
        value ^= (third << 18) & 0xFFFFFFFF
        value = (value + (value >> 11)) & 0xFFFFFFFF
    elif remaining == 2:
        value = (value + data[offset] + (data[offset + 1] << 8)) & 0xFFFFFFFF
        value ^= (value << 11) & 0xFFFFFFFF
        value = (value + (value >> 17)) & 0xFFFFFFFF
    elif remaining == 1:
        byte = data[offset] - 256 if data[offset] >= 128 else data[offset]
        value = (value + byte) & 0xFFFFFFFF
        value ^= (value << 10) & 0xFFFFFFFF
        value = (value + (value >> 1)) & 0xFFFFFFFF
    value ^= (value << 3) & 0xFFFFFFFF
    value = (value + (value >> 5)) & 0xFFFFFFFF
    value ^= (value << 4) & 0xFFFFFFFF
    value = (value + (value >> 17)) & 0xFFFFFFFF
    value ^= (value << 25) & 0xFFFFFFFF
    value = (value + (value >> 6)) & 0xFFFFFFFF
    return value


def compile_entries(entries: Iterable[tuple[str, str]]) -> bytes:
    strings = bytearray()
    index: list[tuple[int, int, int, int]] = []
    keys: set[int] = set()
    for msgid, msgstr in entries:
        key_hash = super_fast_hash(msgid)
        value_hash = super_fast_hash(msgstr)
        if key_hash == value_hash:
            continue
        if key_hash in keys:
            raise ValueError(f"duplicate LuCI translation hash for {msgid!r}")
        keys.add(key_hash)
        encoded = msgstr.encode("utf-8")
        offset = len(strings)
        strings.extend(encoded)
        strings.extend(b"\0" * ((-len(encoded)) % 4))
        index.append((key_hash, value_hash, offset, len(encoded)))
    if not index:
        raise ValueError("PO catalog contains no translated messages")
    output = bytearray(strings)
    for entry in sorted(index):
        output.extend(struct.pack(">IIII", *entry))
    output.extend(struct.pack(">I", len(strings)))
    return bytes(output)


def compile_po(path: Path) -> bytes:
    return compile_entries(parse_po(path))


def main() -> None:
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("input", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.write_bytes(compile_po(args.input))


if __name__ == "__main__":
    main()
