#!/usr/bin/env python3

import argparse
import json
from pathlib import Path


parser = argparse.ArgumentParser()
parser.add_argument("--input", type=Path, required=True)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument(
    "--mode",
    choices=("root-signature", "certificate", "request"),
    default="root-signature",
)
args = parser.parse_args()

response = json.loads(args.input.read_text())
if args.mode == "root-signature":
    signature = response["deployment_trust"]["signature_hex"]
    response["deployment_trust"]["signature_hex"] = (
        ("00" if signature[:2] != "00" else "01") + signature[2:]
    )
elif args.mode == "certificate":
    response["management_certificate_pem"] = response["business_certificate_pem"]
else:
    nonce = response["approval"]["request"]["nonce"]
    response["approval"]["request"]["nonce"] = (
        ("00" if nonce[:2] != "00" else "01") + nonce[2:]
    )
args.output.write_text(json.dumps(response, indent=2) + "\n")
