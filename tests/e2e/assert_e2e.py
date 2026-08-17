#!/usr/bin/env python3
import gzip
import http.client
import json
from pathlib import Path
import re
import socket
import ssl
import struct
import subprocess
import time
import uuid

AUTHORIZATION = "Bearer flowsplice-e2e-administrator-token"
CREDENTIAL_ID = "11111111-1111-4111-8111-111111111111"
COMPOSE_FILE = Path(__file__).resolve().parent / "compose.yaml"


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
            relay_status, _, relay_body = http_get("/api/relays", {"Accept": "application/json"})
            if status == 200 and catalog_status == 200 and relay_status == 200:
                state = json.loads(body)
                catalog = json.loads(catalog_body)
                directory = json.loads(relay_body)
                if (
                    state["ok"]
                    and catalog["home_alias"] == "E2E Home"
                    and len(catalog["services"]) == 2
                    and directory["generation"] >= 1
                    and {relay["id"] for relay in directory["relays"]}
                    == {"relay-1", "relay-2"}
                ):
                    return state
        except Exception as error:  # startup polling deliberately records all transport failures
            last_error = error
        time.sleep(1)
    raise AssertionError(f"Travel Agent did not become ready: {last_error}")


def read_line(stream: socket.socket) -> bytes:
    received = bytearray()
    while not received.endswith(b"\n"):
        chunk = stream.recv(4096)
        if not chunk:
            raise AssertionError("TCP flow closed before a complete response")
        received.extend(chunk)
    return bytes(received)


def exchange_line(stream: socket.socket, payload: bytes) -> int:
    stream.sendall(payload + b"\n")
    response = read_line(stream)
    connection, echoed = response.split(b":", 1)
    assert echoed == payload + b"\n", (echoed, payload)
    return int(connection)


def wait_active_relay(excluded: str | None = None) -> str:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        status, _, body = http_get("/api/status", {"Accept": "application/json"})
        if status == 200:
            active = json.loads(body)["active_relays"]
            if len(active) == 1 and active[0] != excluded:
                return active[0]
        time.sleep(0.2)
    raise AssertionError(f"Travel did not select a replacement Relay; excluded={excluded}")


def check_tcp_relay_failover() -> tuple[str, str]:
    with socket.create_connection(("127.0.0.1", 11080), timeout=5) as stream:
        stream.settimeout(30)
        connection_id = exchange_line(stream, b"before-relay-failure")
        active = wait_active_relay()
        time.sleep(3)
        assert exchange_line(stream, b"after-stable-reevaluation") == connection_id
        assert wait_active_relay() == active
        service = {"relay-1": "relay1", "relay-2": "relay2"}[active]
        subprocess.run(
            ["docker", "compose", "-f", str(COMPOSE_FILE), "kill", "-s", "KILL", service],
            check=True,
        )

        for sequence in range(32):
            observed = exchange_line(stream, f"after-relay-failure-{sequence:04d}".encode())
            assert observed == connection_id, (observed, connection_id)

        replacement = wait_active_relay(excluded=active)
        assert replacement != active
        return active, replacement


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


def write_control_frame(stream: ssl.SSLSocket, message: dict) -> None:
    payload = json.dumps(message, separators=(",", ":")).encode()
    stream.sendall(struct.pack("!I", len(payload)) + payload)


def read_control_frame(stream: ssl.SSLSocket) -> dict:
    prefix = bytearray()
    while len(prefix) < 4:
        chunk = stream.recv(4 - len(prefix))
        if not chunk:
            raise AssertionError("Relay closed before the control-frame length")
        prefix.extend(chunk)
    length = struct.unpack("!I", prefix)[0]
    assert 0 < length <= 262_144, length
    payload = bytearray()
    while len(payload) < length:
        chunk = stream.recv(length - len(payload))
        if not chunk:
            raise AssertionError("Relay closed before the complete control frame")
        payload.extend(chunk)
    return json.loads(payload)


def check_duplicate_travel_login_is_rejected() -> None:
    context = management_tls_context(ssl.TLSVersion.TLSv1_3)
    for port, server_name in [
        (18443, "relay-1.flowsplice"),
        (28443, "relay-2.flowsplice"),
    ]:
        with socket.create_connection(("127.0.0.1", port), timeout=3) as raw:
            with context.wrap_socket(raw, server_hostname=server_name) as stream:
                stream.settimeout(5)
                write_control_frame(
                    stream,
                    {
                        "type": "travel_hello",
                        "id": "travel-1",
                        "session_id": str(uuid.uuid4()),
                        "purpose": "catalog",
                    },
                )
                response = read_control_frame(stream)
                assert response["type"] == "travel_hello_denied", response
                assert "already online" in response["reason"], response


def server_admin(*arguments: str) -> dict:
    result = subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "exec",
            "-T",
            "server",
            "/usr/local/bin/flowsplice-server",
            "--config",
            "/config/server.toml",
            *arguments,
        ],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return json.loads(result.stdout.strip().splitlines()[-1])


def container_pid(service: str) -> int:
    container_id = subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "ps", "-q", service],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()
    assert container_id, service
    return int(
        subprocess.run(
            ["docker", "inspect", "-f", "{{.State.Pid}}", container_id],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        ).stdout.strip()
    )


def wait_authorization_acks(generation: int) -> dict:
    expected = {"home:home-1", "relay:relay-1", "relay:relay-2"}
    deadline = time.monotonic() + 30
    last = None
    while time.monotonic() < deadline:
        last = server_admin("--travel-authorization-status")
        acknowledgements = last["acknowledgements"]
        if all(acknowledgements.get(node) == generation for node in expected):
            return last
        time.sleep(0.5)
    raise AssertionError(f"authorization generation {generation} was not acknowledged: {last}")


def assert_revoked_management_certificate_rejected(port: int, server_name: str) -> None:
    context = management_tls_context(ssl.TLSVersion.TLSv1_3)
    with socket.create_connection(("127.0.0.1", port), timeout=3) as raw:
        try:
            with context.wrap_socket(raw, server_hostname=server_name) as stream:
                stream.settimeout(3)
                write_control_frame(
                    stream,
                    {
                        "type": "travel_hello",
                        "id": "travel-1",
                        "session_id": str(uuid.uuid4()),
                        "purpose": "catalog",
                    },
                )
                response = read_control_frame(stream)
                if response["type"] == "travel_hello_denied":
                    return
                raise AssertionError(f"Relay {server_name} accepted revoked Travel: {response}")
        except AssertionError as error:
            if "closed before" in str(error):
                return
            raise
        except (ConnectionError, ssl.SSLError):
            return
    raise AssertionError(f"Relay {server_name} accepted a revoked Travel certificate")


def wait_local_flow_closed(stream: socket.socket) -> None:
    deadline = time.monotonic() + 50
    stream.settimeout(1)
    while time.monotonic() < deadline:
        try:
            stream.sendall(b"revoked-flow-must-close\n")
            if not stream.recv(4096):
                return
        except (ConnectionError, OSError, socket.timeout):
            try:
                stream.settimeout(0.1)
                if not stream.recv(1):
                    return
            except socket.timeout:
                stream.settimeout(1)
                continue
            except (ConnectionError, OSError):
                return
    raise AssertionError("revoked Travel flow remained open past recovery timeout")


def check_live_revocation(failed_relay: str) -> None:
    service = {"relay-1": "relay1", "relay-2": "relay2"}[failed_relay]
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d", service],
        check=True,
    )
    wait_authorization_acks(1)
    pids = {
        service_name: container_pid(service_name)
        for service_name in ("server", "relay1", "relay2", "homeagent")
    }

    with socket.create_connection(("127.0.0.1", 11080), timeout=5) as stream:
        stream.settimeout(10)
        exchange_line(stream, b"before-live-revocation")
        response = server_admin(
            "--revoke-travel-credential",
            CREDENTIAL_ID,
            "--revocation-reason",
            "E2E revocation",
        )
        assert response["ok"] and response["changed"] and response["generation"] == 2, response
        wait_authorization_acks(2)
        wait_local_flow_closed(stream)

    duplicate = server_admin(
        "--revoke-travel-credential",
        CREDENTIAL_ID,
        "--revocation-reason",
        "idempotency check",
    )
    assert duplicate["ok"] and not duplicate["changed"] and duplicate["generation"] == 2
    assert_revoked_management_certificate_rejected(18443, "relay-1.flowsplice")
    assert_revoked_management_certificate_rejected(28443, "relay-2.flowsplice")
    assert pids == {
        service_name: container_pid(service_name)
        for service_name in ("server", "relay1", "relay2", "homeagent")
    }
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "restart", "relay2"],
        check=True,
    )
    wait_authorization_acks(2)
    assert_revoked_management_certificate_rejected(28443, "relay-2.flowsplice")


def check_tls_policy_and_slow_loris_deadline() -> None:
    tls12 = management_tls_context(ssl.TLSVersion.TLSv1_2)
    try:
        with socket.create_connection(("127.0.0.1", 18443), timeout=3) as raw:
            with tls12.wrap_socket(raw, server_hostname="relay-1.flowsplice"):
                pass
    except (OSError, ssl.SSLError):
        pass
    else:
        raise AssertionError("Relay unexpectedly accepted TLS 1.2")

    tls13 = management_tls_context(ssl.TLSVersion.TLSv1_3)
    with socket.create_connection(("127.0.0.1", 18443), timeout=3) as raw:
        with tls13.wrap_socket(raw, server_hostname="relay-1.flowsplice") as stream:
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
check_udp()
check_embedded_spa()
check_duplicate_travel_login_is_rejected()
check_tls_policy_and_slow_loris_deadline()
failed_relay, replacement_relay = check_tcp_relay_failover()
check_live_revocation(failed_relay)
checks = [
    "two-relay-directory",
    "same-tcp-flow-relay-failover",
    "same-home-target-connection",
    "udp",
    "embedded-spa",
    "tls13-only",
    "slow-frame-deadline",
    "duplicate-travel-login-rejected-across-relays",
    "live-revocation-three-node-ack",
    "live-revocation-closes-existing-flow",
    "revoked-certificate-rejected-by-both-relays",
    "revocation-without-process-restart",
    "duplicate-revocation-idempotent",
    "restarted-relay-retains-revocation",
]
print(
    json.dumps(
        {
            "result": "ok",
            "travel": state["travel_id"],
            "failed_relay": failed_relay,
            "replacement_relay": replacement_relay,
            "checks": checks,
        }
    )
)
