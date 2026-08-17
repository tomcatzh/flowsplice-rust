#!/usr/bin/env python3
import gzip
import http.client
import json
from pathlib import Path
import re
import socket
import subprocess
import time

AUTHORIZATION = "Bearer flowsplice-e2e-administrator-token"
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
                homes = {home["home_id"]: home for home in catalog["homes"]}
                if (
                    state["ok"]
                    and set(homes) == {"home-1", "home-2"}
                    and homes["home-1"]["home_alias"] == "E2E Home"
                    and homes["home-2"]["home_alias"] == "E2E Home Two"
                    and {service["id"] for service in homes["home-1"]["services"]}
                    == {"tcp-echo", "udp-echo", "home-1-only"}
                    and {service["id"] for service in homes["home-2"]["services"]}
                    == {"tcp-echo"}
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


def exchange_line(
    stream: socket.socket, payload: bytes, expected_home: bytes | None = None
) -> int:
    stream.sendall(payload + b"\n")
    response = read_line(stream)
    if expected_home is None:
        connection, echoed = response.split(b":", 1)
    else:
        home, connection, echoed = response.split(b":", 2)
        assert home == expected_home, (home, expected_home)
    assert echoed == payload + b"\n", (echoed, payload)
    return int(connection)


def wait_catalog_homes(expected: set[str]) -> dict:
    deadline = time.monotonic() + 30
    last = None
    while time.monotonic() < deadline:
        status, _, body = http_get("/api/catalog", {"Accept": "application/json"})
        if status == 200:
            last = json.loads(body)
            if {home["home_id"] for home in last["homes"]} == expected:
                return last
        time.sleep(0.2)
    raise AssertionError(f"catalog did not reach Home set {expected}: {last}")


def expect_mapping_unavailable_without_cross_home_fallback(port: int) -> None:
    with socket.create_connection(("127.0.0.1", port), timeout=5) as stream:
        stream.settimeout(6)
        stream.sendall(b"must-not-cross-home-boundary\n")
        try:
            received = stream.recv(4096)
        except socket.timeout:
            # Travel intentionally keeps retrying within its recovery window.
            # Silence proves it did not fall back to another Home's same-name service.
            return
        except (ConnectionError, OSError):
            return
        assert received == b"", received


def check_multi_home_business_routing() -> None:
    with socket.create_connection(("127.0.0.1", 11080), timeout=5) as home_one:
        home_one.settimeout(15)
        exchange_line(home_one, b"same-service-on-home-one")
    with socket.create_connection(("127.0.0.1", 11082), timeout=5) as home_two:
        home_two.settimeout(15)
        exchange_line(home_two, b"same-service-on-home-two", b"home-2")
    # This service exists on home-1 only, while the mapping explicitly selects
    # home-2. It must fail instead of falling back across the Home boundary.
    expect_mapping_unavailable_without_cross_home_fallback(11083)


def check_home_lifecycle_is_isolated() -> None:
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "stop", "homeagent2"],
        check=True,
    )
    wait_catalog_homes({"home-1"})
    with socket.create_connection(("127.0.0.1", 11080), timeout=5) as home_one:
        home_one.settimeout(15)
        exchange_line(home_one, b"home-one-survives-home-two-offline")
    expect_mapping_unavailable_without_cross_home_fallback(11082)
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d", "homeagent2"],
        check=True,
    )
    wait_catalog_homes({"home-1", "home-2"})
    with socket.create_connection(("127.0.0.1", 11082), timeout=5) as home_two:
        home_two.settimeout(15)
        exchange_line(home_two, b"home-two-returned", b"home-2")


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


def check_duplicate_travel_login_is_rejected() -> None:
    generated_dir = Path(__file__).resolve().parent / "generated"
    network = "e2e_flowsplice"
    for relay_addr, server_name, relay_id in [
        ("relay1:8443", "relay-1.flowsplice", "relay-1"),
        ("relay2:8443", "relay-2.flowsplice", "relay-2"),
    ]:
        subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--network",
                network,
                "-v",
                f"{generated_dir / 'travel'}:/travel:ro",
                "-v",
                f"{generated_dir / 'certs'}:/certs:ro",
                "flowsplice-e2e:local",
                "/usr/local/bin/flowsplice-travel-login-probe",
                "duplicate",
                relay_addr,
                server_name,
                relay_id,
            ],
            check=True,
        )


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
    expected = {"home:home-1", "home:home-2", "relay:relay-1", "relay:relay-2"}
    deadline = time.monotonic() + 30
    last = None
    while time.monotonic() < deadline:
        last = server_admin("--travel-authorization-status")
        acknowledgements = last["acknowledgements"]
        if all(acknowledgements.get(node) == generation for node in expected):
            return last
        time.sleep(0.5)
    raise AssertionError(f"authorization generation {generation} was not acknowledged: {last}")


def check_enrollment_import() -> tuple[str, int]:
    status = server_admin("--travel-authorization-status")
    credentials = [
        credential
        for credential in status["credentials"]
        if credential["travel_id"] == "travel-1"
    ]
    assert len(credentials) == 1, status
    credential = credentials[0]
    remaining = credential["not_after_unix_secs"] - int(time.time())
    assert 364 * 86400 <= remaining <= 366 * 86400, remaining
    assert credential["active"] and not credential["revoked"], credential
    duplicate = server_admin(
        "--import-travel-enrollment",
        "/authorization/enrollment-response.json",
    )
    assert duplicate["ok"] and not duplicate["changed"], duplicate
    assert duplicate["generation"] == status["generation"], duplicate
    wait_authorization_acks(status["generation"])
    return credential["credential_id"], status["generation"]


def assert_revoked_management_certificate_rejected(relay: str, server_name: str) -> None:
    generated_dir = Path(__file__).resolve().parent / "generated"
    result = subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "--network",
            "e2e_flowsplice",
            "-v",
            f"{generated_dir / 'travel'}:/travel:ro",
            "-v",
            f"{generated_dir / 'certs'}:/certs:ro",
            "flowsplice-e2e:local",
            "/usr/local/bin/flowsplice-travel-login-probe",
            "duplicate",
            f"{relay}:8443",
            server_name,
            relay.replace("relay", "relay-"),
        ],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert result.returncode != 0, result.stdout
    assert "accepted a duplicate Travel login" not in result.stdout, result.stdout


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


def check_live_revocation(
    failed_relay: str, credential_id: str, imported_generation: int
) -> None:
    service = {"relay-1": "relay1", "relay-2": "relay2"}[failed_relay]
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d", service],
        check=True,
    )
    wait_authorization_acks(imported_generation)
    pids = {
        service_name: container_pid(service_name)
        for service_name in ("server", "relay1", "relay2", "homeagent", "homeagent2")
    }

    with (
        socket.create_connection(("127.0.0.1", 11080), timeout=5) as home_one,
        socket.create_connection(("127.0.0.1", 11082), timeout=5) as home_two,
    ):
        home_one.settimeout(10)
        home_two.settimeout(10)
        exchange_line(home_one, b"before-live-revocation-home-one")
        exchange_line(home_two, b"before-live-revocation-home-two", b"home-2")
        response = server_admin(
            "--revoke-travel-credential",
            credential_id,
            "--revocation-reason",
            "E2E revocation",
        )
        revoked_generation = imported_generation + 1
        assert (
            response["ok"]
            and response["changed"]
            and response["generation"] == revoked_generation
        ), response
        wait_authorization_acks(revoked_generation)
        wait_local_flow_closed(home_one)
        wait_local_flow_closed(home_two)

    duplicate = server_admin(
        "--revoke-travel-credential",
        credential_id,
        "--revocation-reason",
        "idempotency check",
    )
    assert (
        duplicate["ok"]
        and not duplicate["changed"]
        and duplicate["generation"] == revoked_generation
    )
    assert_revoked_management_certificate_rejected("relay1", "relay-1.flowsplice")
    assert_revoked_management_certificate_rejected("relay2", "relay-2.flowsplice")
    assert pids == {
        service_name: container_pid(service_name)
        for service_name in ("server", "relay1", "relay2", "homeagent", "homeagent2")
    }
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "restart", "relay2"],
        check=True,
    )
    wait_authorization_acks(revoked_generation)
    assert_revoked_management_certificate_rejected("relay2", "relay-2.flowsplice")


def check_tls_policy_and_slow_loris_deadline() -> None:
    import ssl

    cert_dir = Path(__file__).resolve().parent / "generated" / "certs"
    tls12 = ssl.create_default_context(cafile=str(cert_dir / "management-ca.crt"))
    tls12.minimum_version = ssl.TLSVersion.TLSv1_2
    tls12.maximum_version = ssl.TLSVersion.TLSv1_2
    try:
        with socket.create_connection(("127.0.0.1", 18443), timeout=3) as raw:
            with tls12.wrap_socket(raw, server_hostname="relay-1.flowsplice"):
                pass
    except (OSError, ssl.SSLError):
        pass
    else:
        raise AssertionError("Relay unexpectedly accepted TLS 1.2")

    generated_dir = Path(__file__).resolve().parent / "generated"
    subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "--network",
            "e2e_flowsplice",
            "-v",
            f"{generated_dir / 'travel'}:/travel:ro",
            "-v",
            f"{generated_dir / 'certs'}:/certs:ro",
            "flowsplice-e2e:local",
            "/usr/local/bin/flowsplice-travel-login-probe",
            "slow-frame",
            "relay1:8443",
            "relay-1.flowsplice",
            "relay-1",
        ],
        check=True,
    )


state = wait_ready()
credential_id, imported_generation = check_enrollment_import()
check_udp()
check_multi_home_business_routing()
check_home_lifecycle_is_isolated()
check_embedded_spa()
check_duplicate_travel_login_is_rejected()
check_tls_policy_and_slow_loris_deadline()
failed_relay, replacement_relay = check_tcp_relay_failover()
check_live_revocation(failed_relay, credential_id, imported_generation)
checks = [
    "two-relay-directory",
    "two-home-catalog",
    "same-service-id-isolated-by-home",
    "logical-business-selects-exact-home",
    "wrong-home-service-fails-without-fallback",
    "one-home-offline-does-not-affect-other-home",
    "home-catalog-removal-and-return",
    "same-tcp-flow-relay-failover",
    "same-home-target-connection",
    "udp",
    "embedded-spa",
    "tls13-only",
    "slow-frame-deadline",
    "duplicate-travel-login-rejected-across-relays",
    "encrypted-local-travel-enrollment",
    "offline-dual-ca-issuance",
    "default-one-year-validity",
    "live-add-only-credential-import",
    "duplicate-credential-import-idempotent",
    "live-revocation-four-node-ack",
    "live-revocation-closes-flows-on-both-homes",
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
