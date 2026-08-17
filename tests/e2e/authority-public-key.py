#!/usr/bin/env python3
import argparse
from pathlib import Path
import subprocess


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--authority-key", required=True, type=Path)
    parser.add_argument("--password-file", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    public_der = subprocess.run(
        [
            "openssl",
            "pkey",
            "-in",
            str(args.authority_key),
            "-passin",
            f"file:{args.password_file}",
            "-pubout",
            "-outform",
            "DER",
        ],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    ).stdout
    public_key = public_der[-65:]
    if len(public_key) != 65 or public_key[0] != 4:
        raise RuntimeError("OpenSSL did not produce an uncompressed P-256 public key")
    args.output.write_text(public_key.hex() + "\n", encoding="ascii")


if __name__ == "__main__":
    main()
