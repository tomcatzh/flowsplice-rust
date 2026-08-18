#!/usr/bin/env python3
import argparse
import http.client
import json
from pathlib import Path
import sys

TOKEN = "flowsplice-e2e-home-issuer-administrator-token"


def request(port: int, method: str, path: str, body=None):
    encoded = None if body is None else json.dumps(body).encode()
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
    connection.request(
        method,
        path,
        body=encoded,
        headers={
            "Authorization": f"Bearer {TOKEN}",
            "Accept": "application/json",
            "Content-Type": "application/json",
        },
    )
    response = connection.getresponse()
    raw = response.read()
    connection.close()
    decoded = json.loads(raw or b"{}")
    if response.status >= 400:
        raise RuntimeError(decoded.get("error", f"HTTP {response.status}"))
    return decoded


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=["status", "issue"])
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--request")
    parser.add_argument("--password-file")
    parser.add_argument("--scope", choices=["global", "home", "service"])
    parser.add_argument("--service-id")
    parser.add_argument("--protocol", choices=["tcp", "udp"], default="tcp")
    parser.add_argument("--valid-days", type=int)
    parser.add_argument("--valid-minutes", type=int)
    parser.add_argument("--output")
    parser.add_argument("--expect-failure", action="store_true")
    args = parser.parse_args()

    if args.action == "status":
        print(json.dumps(request(args.port, "GET", "/api/status")))
        return 0

    status = request(args.port, "GET", "/api/status")
    if not all([args.request, args.password_file, args.scope, args.output]):
        parser.error("issue requires --request, --password-file, --scope, and --output")
    if args.scope == "global":
        scope = {"kind": "global"}
    elif args.scope == "home":
        scope = {"kind": "home", "home_id": status["home_id"]}
    else:
        if not args.service_id:
            parser.error("service scope requires --service-id")
        scope = {
            "kind": "service",
            "home_id": status["home_id"],
            "service_id": args.service_id,
            "protocol": args.protocol,
        }
    try:
        validity = {}
        if args.valid_days is not None:
            validity["valid_days"] = args.valid_days
        if args.valid_minutes is not None:
            validity["valid_minutes"] = args.valid_minutes
        result = request(
            args.port,
            "POST",
            "/api/issue",
            {
                "request": json.loads(Path(args.request).read_text()),
                **validity,
                "scope": scope,
                "password": Path(args.password_file).read_text().rstrip("\r\n"),
            },
        )
    except RuntimeError as error:
        if args.expect_failure:
            print(str(error))
            return 0
        raise
    if args.expect_failure:
        raise RuntimeError("Home issuer unexpectedly accepted the request")
    Path(args.output).write_text(json.dumps(result["enrollment"], indent=2) + "\n")
    print(json.dumps({"generation": result["generation"]}))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)
