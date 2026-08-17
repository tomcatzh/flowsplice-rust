#!/usr/bin/env python3
import gzip
import http.client
import json
from pathlib import Path
import re
import socket
import ssl
import time

AUTHORIZATION = "Bearer flowsplice-e2e-administrator-token"


def http_get(path: str, headers: dict[str, str] | None = None):
    request_headers = {"Authorization": AUTHORIZATION}
    request_headers.update(headers or {})
    conn = http.client.HTTPConnection("127.0.0.1", 19080, timeout=3)
    conn.request("GET", path, headers=request_headers)
    response = conn.getresponse()
    body = response.read()
    result = response.status, {key.lower(): value for key, value in response.getheaders()}, body
    conn.close()
    return result


def wait_ready() -> dict:
    deadline = time.monotonic() + 90
    last_error = None
    while time.monotonic() < deadline:
        try:
            status, _, body = http_get("/api/status", {"Accept": "application/json"})
            catalog_status, _, catalog_body = http_get("/api/catalog", {"Accept": "application/json"})
            if status == 200 and catalog_status == 200:
                state = json.loads(body)
                catalog = json.loads(catalog_body)
                if (
                    state["ok"]
                    and catalog["home_alias"] == "E2E Home"
                    and len(catalog["services"]) == 2
                ):
                    return state
        except Exception as error:  # startup polling deliberately records all transport failures
            last_error = error
        time.sleep(1)
    raise AssertionError(f"Travel Agent did not become ready: {last_error}")


def check_tcp() -> None:
    expected = b"flowsplice-tcp-e2e-" + bytes(range(64))
    with socket.create_connection(("127.0.0.1", 11080), timeout=5) as stream:
        stream.sendall(expected)
        stream.shutdown(socket.SHUT_WR)
        received = bytearray()
        while len(received) < len(expected):
            chunk = stream.recv(4096)
            if not chunk:
                break
            received.extend(chunk)
    assert bytes(received) == expected, (len(received), len(expected))


def check_udp() -> None:
    expected = b"flowsplice-udp-e2e"
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as datagram:
        datagram.settimeout(8)
        datagram.sendto(expected, ("127.0.0.1", 11081))
        received, _ = datagram.recvfrom(65535)
    assert received == expected


def check_embedded_spa() -> None:
    status, identity_headers, body = http_get("/", {"Accept": "text/html", "Accept-Encoding": "identity"})
    assert status == 200
    assert b"FlowSplice Travel Agent" in body
    asset_match = re.search(rb'(?:src|href)="(/assets/[^"]+\.(?:js|css))"', body)
    assert asset_match, body[:200]
    asset = asset_match.group(1).decode()

    gzip_status, gzip_headers, gzip_body = http_get(asset, {"Accept-Encoding": "gzip"})
    br_status, br_headers, br_body = http_get(asset, {"Accept-Encoding": "br"})
    assert gzip_status == br_status == 200
    assert gzip_headers.get("content-encoding") == "gzip"
    assert br_headers.get("content-encoding") == "br"
    assert gzip_headers.get("etag") != br_headers.get("etag")
    assert gzip.decompress(gzip_body)
    assert br_body
    assert identity_headers.get("cache-control")

    api_status, _, _ = http_get("/api/not-a-route", {"Accept": "text/html"})
    asset_status, _, _ = http_get("/assets/not-a-real-hash.js", {"Accept": "text/html"})
    assert api_status == 404
    assert asset_status == 404


def management_tls_context(version: ssl.TLSVersion) -> ssl.SSLContext:
    cert_dir = Path(__file__).resolve().parent / "generated" / "certs"
    context = ssl.create_default_context(cafile=str(cert_dir / "management-ca.crt"))
    context.minimum_version = version
    context.maximum_version = version
    context.load_cert_chain(
        certfile=str(cert_dir / "travel-management.crt"),
        keyfile=str(cert_dir / "travel-management.key"),
    )
    return context


def check_tls_policy_and_slow_loris_deadline() -> None:
    tls12 = management_tls_context(ssl.TLSVersion.TLSv1_2)
    try:
        with socket.create_connection(("127.0.0.1", 18443), timeout=3) as raw:
            with tls12.wrap_socket(raw, server_hostname="relay.flowsplice"):
                pass
    except (OSError, ssl.SSLError):
        pass
    else:
        raise AssertionError("Relay unexpectedly accepted TLS 1.2")

    tls13 = management_tls_context(ssl.TLSVersion.TLSv1_3)
    with socket.create_connection(("127.0.0.1", 18443), timeout=3) as raw:
        with tls13.wrap_socket(raw, server_hostname="relay.flowsplice") as stream:
            assert stream.version() == "TLSv1.3"
            stream.settimeout(13)
            stream.sendall(b"\x00\x00")
            started = time.monotonic()
            try:
                closed = stream.recv(1) == b""
            except ssl.SSLEOFError:
                closed = True
            elapsed = time.monotonic() - started
            assert closed, "Relay did not close the incomplete control frame"
            assert elapsed < 12, f"slow control frame survived too long: {elapsed:.2f}s"


state = wait_ready()
check_tcp()
check_udp()
check_embedded_spa()
check_tls_policy_and_slow_loris_deadline()
checks = ["tcp", "udp", "embedded-spa", "tls13-only", "slow-frame-deadline"]
print(json.dumps({"result": "ok", "travel": state["travel_id"], "checks": checks}))
