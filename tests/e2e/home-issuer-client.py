#!/usr/bin/env python3
import argparse
import http.client
import json
from pathlib import Path
import sys
import time

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
    parser.add_argument(
        "action",
        choices=[
            "status",
            "issue",
            "pending",
            "approve",
            "home-pending",
            "home-approve",
            "revoke",
        ],
    )
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--request")
    parser.add_argument("--password-file")
    parser.add_argument("--scope", choices=["global", "home", "service"])
    parser.add_argument("--service-id")
    parser.add_argument("--protocol", choices=["tcp", "udp"], default="tcp")
    parser.add_argument("--valid-days", type=int)
    parser.add_argument("--valid-minutes", type=int)
    parser.add_argument("--output")
    parser.add_argument("--request-id")
    parser.add_argument("--credential-id")
    parser.add_argument("--travel-id")
    parser.add_argument("--home-id")
    parser.add_argument(
        "--profile", choices=["serving_only", "home_issuer", "global_issuer"]
    )
    parser.add_argument("--wait-secs", type=int, default=0)
    parser.add_argument("--reason", default="E2E revocation")
    parser.add_argument("--expect-failure", action="store_true")
    args = parser.parse_args()

    if args.action == "status":
        print(json.dumps(request(args.port, "GET", "/api/status")))
        return 0

    if args.action == "pending":
        deadline = time.monotonic() + args.wait_secs
        while True:
            records = request(args.port, "GET", "/api/enrollment/pending")["items"]
            if args.travel_id:
                record = next(
                    (item for item in records if item["travel_id"] == args.travel_id),
                    None,
                )
                if record is not None:
                    print(json.dumps(record))
                    return 0
            else:
                print(json.dumps(records))
                return 0
            if time.monotonic() >= deadline:
                raise RuntimeError(
                    f"remote enrollment for {args.travel_id} did not arrive"
                )
            time.sleep(1)

    if args.action == "home-pending":
        deadline = time.monotonic() + args.wait_secs
        while True:
            records = request(args.port, "GET", "/api/home-enrollment/pending")["items"]
            if args.home_id:
                record = next(
                    (item for item in records if item["home_id"] == args.home_id),
                    None,
                )
                if record is not None:
                    print(json.dumps(record))
                    return 0
            else:
                print(json.dumps(records))
                return 0
            if time.monotonic() >= deadline:
                raise RuntimeError(f"Home enrollment for {args.home_id} did not arrive")
            time.sleep(1)

    if args.action == "home-approve":
        if not all([args.request_id, args.password_file, args.profile]):
            parser.error(
                "home-approve requires --request-id, --password-file, and --profile"
            )
        body = {
            "request_id": args.request_id,
            "profile": args.profile,
            "password": Path(args.password_file).read_text().rstrip("\r\n"),
        }
        if args.valid_days is not None:
            body["valid_days"] = args.valid_days
        try:
            result = request(args.port, "POST", "/api/home-enrollment/approve", body)
        except RuntimeError as error:
            if args.expect_failure:
                print(str(error))
                return 0
            raise
        if args.expect_failure:
            raise RuntimeError("Home issuer unexpectedly approved the new Home")
        print(json.dumps(result))
        return 0

    if args.action == "revoke":
        if not all([args.credential_id, args.password_file]):
            parser.error("revoke requires --credential-id and --password-file")
        body = {
            "credential_id": args.credential_id,
            "reason": args.reason,
            "password": Path(args.password_file).read_text().rstrip("\r\n"),
        }
        try:
            result = request(args.port, "POST", "/api/revoke", body)
        except RuntimeError as error:
            if args.expect_failure:
                print(str(error))
                return 0
            raise
        if args.expect_failure:
            raise RuntimeError("Home issuer unexpectedly accepted the revocation")
        print(json.dumps(result))
        return 0

    status = request(args.port, "GET", "/api/status")
    if args.action == "approve":
        if not all([args.request_id, args.password_file, args.scope]):
            parser.error("approve requires --request-id, --password-file, and --scope")
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
        body = {
            "request_id": args.request_id,
            "scope": scope,
            "password": Path(args.password_file).read_text().rstrip("\r\n"),
        }
        if args.valid_days is not None:
            body["valid_days"] = args.valid_days
        if args.valid_minutes is not None:
            body["valid_minutes"] = args.valid_minutes
        try:
            result = request(args.port, "POST", "/api/enrollment/approve", body)
        except RuntimeError as error:
            if args.expect_failure:
                print(str(error))
                return 0
            raise
        if args.expect_failure:
            raise RuntimeError("Home issuer unexpectedly approved the request")
        if args.output:
            Path(args.output).write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result))
        return 0

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
            "/api/test/issue",
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
