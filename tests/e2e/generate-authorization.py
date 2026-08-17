#!/usr/bin/env python3
import argparse
import json
from pathlib import Path
import subprocess
import time


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--authority-key", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--credential-id", required=True)
    parser.add_argument("--travel-id", required=True)
    parser.add_argument("--management-pin", required=True)
    parser.add_argument("--business-pin", required=True)
    args = parser.parse_args()

    now = int(time.time())
    payload = json.dumps(
        {
            "credential_id": args.credential_id,
            "travel_id": args.travel_id,
            "management_spki_sha256": args.management_pin,
            "business_spki_sha256": args.business_pin,
            "not_before_unix_secs": now - 60,
            "not_after_unix_secs": now + 86400,
        },
        separators=(",", ":"),
    ).encode()
    signature = subprocess.run(
        [
            "openssl",
            "dgst",
            "-sha256",
            "-sign",
            str(args.authority_key),
        ],
        input=payload,
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    public_der = subprocess.run(
        [
            "openssl",
            "pkey",
            "-in",
            str(args.authority_key),
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

    args.output_dir.mkdir(parents=True, exist_ok=True)
    (args.output_dir / "credentials.json").write_text(
        json.dumps(
            {
                "credentials": [
                    {
                        "payload_hex": payload.hex(),
                        "signature_hex": signature.hex(),
                    }
                ]
            },
            indent=2,
        )
        + "\n"
    )
    (args.output_dir / "authority-public-key.txt").write_text(public_key.hex() + "\n")


if __name__ == "__main__":
    main()
