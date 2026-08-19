#!/usr/bin/env python3
import argparse
import json
import subprocess
import time
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cert-dir", required=True, type=Path)
    parser.add_argument("--authorization-dir", required=True, type=Path)
    parser.add_argument("--root-dir", required=True, type=Path)
    parser.add_argument("--password-file", required=True, type=Path)
    parser.add_argument("--home1-management-pin", required=True)
    parser.add_argument("--home1-business-pin", required=True)
    parser.add_argument("--home2-management-pin", required=True)
    parser.add_argument("--home2-business-pin", required=True)
    args = parser.parse_args()

    now = int(time.time())
    read = lambda path: path.read_text(encoding="ascii")
    trust = {
        "version": 1,
        "deployment_id": "flowsplice-e2e",
        "generation": 1,
        "not_before_unix_secs": now - 300,
        "not_after_unix_secs": now + 700 * 24 * 60 * 60,
        "management_ca_certificate_pem": read(args.cert_dir / "management-ca.crt"),
        "business_ca_certificate_pem": read(args.cert_dir / "business-ca.crt"),
        "server_control_keys": [
            {
                "server_id": "server-1",
                "epoch": 1,
                "public_key": read(args.authorization_dir / "server-control-public-key.txt").strip(),
            }
        ],
        "home_endpoints": [
            {
                "home_id": "home-1",
                "management_spki_pins": [args.home1_management_pin],
                "business_spki_pins": [args.home1_business_pin],
            },
            {
                "home_id": "home-2",
                "management_spki_pins": [args.home2_management_pin],
                "business_spki_pins": [args.home2_business_pin],
            },
        ],
        "travel_authorities": [
            {
                "kind": "home",
                "id": "home-1-authority",
                "epoch": 1,
                "home_id": "home-1",
                "public_key": read(args.authorization_dir / "home1-authority-public-key.txt").strip(),
            },
            {
                "kind": "home",
                "id": "home-2-authority",
                "epoch": 1,
                "home_id": "home-2",
                "public_key": read(args.authorization_dir / "home2-authority-public-key.txt").strip(),
            },
            {
                "kind": "global",
                "id": "operator-global-authority",
                "epoch": 1,
                "home_id": "home-1",
                "public_key": read(args.authorization_dir / "global-authority-public-key.txt").strip(),
            },
        ],
    }
    payload = json.dumps(trust, separators=(",", ":"), ensure_ascii=True).encode("ascii")
    signature = subprocess.run(
        [
            "openssl",
            "dgst",
            "-sha256",
            "-sign",
            str(args.root_dir / "deployment-root.key"),
            "-passin",
            f"file:{args.password_file}",
        ],
        input=payload,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    ).stdout
    signed = {"payload_hex": payload.hex(), "signature_hex": signature.hex()}
    (args.cert_dir / "deployment-trust.json").write_text(
        json.dumps(signed, indent=2) + "\n", encoding="ascii"
    )


if __name__ == "__main__":
    main()
