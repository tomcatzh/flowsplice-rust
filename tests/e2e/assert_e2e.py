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
ISSUER_AUTHORIZATION = "Bearer flowsplice-e2e-home-issuer-administrator-token"
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


def issuer_get(path: str, headers: dict[str, str] | None = None):
    request_headers = {"Authorization": ISSUER_AUTHORIZATION}
    request_headers.update(headers or {})
    connection = http.client.HTTPConnection("127.0.0.1", 19081, timeout=3)
    connection.request("GET", path, headers=request_headers)
    response = connection.getresponse()
    body = response.read()
    result = (
        response.status,
        {key.lower(): value for key, value in response.getheaders()},
        body,
    )
    connection.close()
    return result


def issuer_request(port: int, method: str, path: str, body=None, expect_ok=True):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=180)
    encoded = None if body is None else json.dumps(body).encode()
    connection.request(
        method,
        path,
        body=encoded,
        headers={
            "Authorization": ISSUER_AUTHORIZATION,
            "Accept": "application/json",
            "Content-Type": "application/json",
        },
    )
    response = connection.getresponse()
    raw = response.read()
    connection.close()
    decoded = json.loads(raw or b"{}")
    if expect_ok:
        assert response.status < 400, (response.status, decoded)
    else:
        assert response.status >= 400, (response.status, decoded)
    return decoded


def travel_request(method: str, path: str, body=None, expect_ok=True):
    connection = http.client.HTTPConnection("127.0.0.1", 19080, timeout=180)
    encoded = None if body is None else json.dumps(body).encode()
    connection.request(
        method,
        path,
        body=encoded,
        headers={
            "Authorization": AUTHORIZATION,
            "Accept": "application/json",
            "Content-Type": "application/json",
        },
    )
    response = connection.getresponse()
    raw = response.read()
    connection.close()
    decoded = json.loads(raw or b"{}")
    if expect_ok:
        assert response.status < 400, (response.status, decoded)
    else:
        assert response.status >= 400, (response.status, decoded)
    return decoded


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


def wait_issuer_ready(port: int) -> None:
    deadline = time.monotonic() + 90
    last_error = None
    while time.monotonic() < deadline:
        try:
            response = issuer_request(port, "GET", "/api/status")
            if response["home_id"] == "home-1":
                return
        except Exception as error:
            last_error = error
        time.sleep(1)
    raise AssertionError(f"Home issuer did not become ready: {last_error}")


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
    travel_asset_body = gzip.decompress(gzip_body)
    assert travel_asset_body
    if asset.endswith(".js"):
        assert b"Travel private-key password" in travel_asset_body
    assert br_body
    assert identity_headers.get("cache-control")

    api_status, _, _ = http_get("/api/not-a-route", {"Accept": "text/html"})
    asset_status, _, _ = http_get("/assets/not-a-real-hash.js", {"Accept": "text/html"})
    assert api_status == 404
    assert asset_status == 404

    issuer_status, issuer_headers, issuer_body = issuer_get(
        "/", {"Accept": "text/html", "Accept-Encoding": "gzip"}
    )
    assert issuer_status == 200
    if issuer_headers.get("content-encoding") == "gzip":
        issuer_body = gzip.decompress(issuer_body)
    assert "FlowSplice · 旅行端签发".encode() in issuer_body

    issuer_asset_match = re.search(
        rb'(?:src|href)="(/assets/[^"]+\.(?:js|css))"', issuer_body
    )
    assert issuer_asset_match, issuer_body[:200]
    issuer_asset = issuer_asset_match.group(1).decode()
    issuer_asset_status, issuer_asset_headers, issuer_asset_body = issuer_get(
        issuer_asset, {"Accept-Encoding": "gzip"}
    )
    assert issuer_asset_status == 200
    assert issuer_asset_headers.get("content-encoding") == "gzip"
    if issuer_asset.endswith(".js"):
        issuer_javascript = gzip.decompress(issuer_asset_body)
        assert "旅行端凭据签发".encode() in issuer_javascript
        assert "更改 Home 签发密码".encode() in issuer_javascript

    issuer_api_status, _, _ = issuer_get(
        "/api/not-a-route", {"Accept": "text/html"}
    )
    issuer_missing_asset_status, _, _ = issuer_get(
        "/assets/not-a-real-hash.js", {"Accept": "text/html"}
    )
    assert issuer_api_status == 404
    assert issuer_missing_asset_status == 404


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


def issue_scope(
    port: int, scope: dict, *, valid_minutes: int | None = None
) -> tuple[str, int]:
    generated = Path(__file__).resolve().parent / "generated"
    issuer_directory = "offline-home2" if port == 29081 else "offline"
    validity = {"valid_days": 365}
    if valid_minutes is not None:
        validity = {"valid_minutes": valid_minutes}
    result = issuer_request(
        port,
        "POST",
        "/api/issue",
        {
            "request": json.loads((generated / "travel/enrollment-request.json").read_text()),
            **validity,
            "scope": scope,
            "password": (generated / issuer_directory / "test-password.txt").read_text().strip(),
        },
    )
    if valid_minutes is not None:
        remaining = result["enrollment"]["approval"]["not_after_unix_secs"] - int(time.time())
        assert valid_minutes * 60 - 5 <= remaining <= valid_minutes * 60 + 5, remaining
    return result["enrollment"]["approval"]["credential_id"], result["generation"]


def revoke_from_home(port: int, credential_id: str, expect_ok=True) -> dict:
    return issuer_request(
        port,
        "POST",
        "/api/revoke",
        {"credential_id": credential_id, "reason": "E2E revocation"},
        expect_ok=expect_ok,
    )


def issued_credentials(port: int) -> list[dict]:
    return issuer_request(port, "GET", "/api/credentials")


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


def check_home_issued_enrollment() -> str:
    credentials = [
        credential for credential in issued_credentials(19081)
        if credential["travel_id"] == "travel-1" and credential["scope"]["kind"] == "global"
    ]
    assert len(credentials) == 1, credentials
    credential = credentials[0]
    remaining = credential["not_after_unix_secs"] - int(time.time())
    assert 364 * 86400 <= remaining <= 366 * 86400, remaining
    assert credential["active"] and not credential["revoked"], credential
    return credential["credential_id"]


def check_private_key_password_rotation() -> dict:
    generated = Path(__file__).resolve().parent / "generated"
    old_password = (generated / "offline/test-password.txt").read_text().strip()
    new_password = "flowsplice-e2e-rotated-private-key-password"
    wrong_password = "flowsplice-e2e-wrong-private-key-password"

    issuer_request(
        19081,
        "POST",
        "/api/private-key-password",
        {"current_password": wrong_password, "new_password": new_password},
        expect_ok=False,
    )
    travel_request(
        "POST",
        "/api/private-key-password",
        {"current_password": wrong_password, "new_password": new_password},
        expect_ok=False,
    )
    home_result = issuer_request(
        19081,
        "POST",
        "/api/private-key-password",
        {"current_password": old_password, "new_password": new_password},
    )
    travel_result = travel_request(
        "POST",
        "/api/private-key-password",
        {"current_password": old_password, "new_password": new_password},
    )
    assert home_result["rotated_keys"] == 4, home_result
    assert travel_result["rotated_keys"] == 2, travel_result

    issuer_request(
        19081,
        "POST",
        "/api/private-key-password",
        {"current_password": old_password, "new_password": wrong_password},
        expect_ok=False,
    )
    travel_request(
        "POST",
        "/api/private-key-password",
        {"current_password": old_password, "new_password": wrong_password},
        expect_ok=False,
    )

    for path in [
        generated / "offline/test-password.txt",
        generated / "travel/test-password.txt",
    ]:
        path.write_text(new_password + "\n")
        path.chmod(0o600)
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "restart", "homeagent", "travelagent"],
        check=True,
    )
    wait_issuer_ready(19081)
    return wait_ready()


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


def wait_scoped_catalog(expected: dict[str, set[str]]) -> None:
    deadline = time.monotonic() + 35
    last = None
    while time.monotonic() < deadline:
        status, _, body = http_get("/api/catalog", {"Accept": "application/json"})
        if status == 200:
            catalog = json.loads(body)
            last = {
                home["home_id"]: {service["id"] for service in home["services"]}
                for home in catalog["homes"]
            }
            if last == expected:
                return
        time.sleep(0.25)
    raise AssertionError(f"scoped catalog did not converge: {last}")


def check_scoped_authorization_and_home_revocation(
    failed_relay: str, global_credential_id: str
) -> None:
    service = {"relay-1": "relay1", "relay-2": "relay2"}[failed_relay]
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d", service],
        check=True,
    )
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

        # These established flows are bound to the global credential. Publish narrower
        # replacement grants only after they exist, then prove revoking the original grant
        # closes its flows while the replacement grants remain usable for new flows.
        home_one_credential, _ = issue_scope(
            19081,
            {
                "kind": "service",
                "home_id": "home-1",
                "service_id": "tcp-echo",
                "protocol": "tcp",
            },
            valid_minutes=30,
        )
        home_two_credential, _ = issue_scope(
            29081, {"kind": "home", "home_id": "home-2"}
        )
        revoke_from_home(29081, global_credential_id, expect_ok=False)
        revoke_from_home(19081, global_credential_id)
        wait_local_flow_closed(home_one)
        wait_local_flow_closed(home_two)

    wait_scoped_catalog(
        {"home-1": {"tcp-echo"}, "home-2": {"tcp-echo"}}
    )
    with socket.create_connection(("127.0.0.1", 11080), timeout=5) as home_one:
        home_one.settimeout(15)
        exchange_line(home_one, b"home-one-service-scope")
    with socket.create_connection(("127.0.0.1", 11082), timeout=5) as home_two:
        home_two.settimeout(15)
        exchange_line(home_two, b"home-two-home-scope", b"home-2")
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as datagram:
        datagram.settimeout(4)
        datagram.sendto(b"must-not-pass-service-scope", ("127.0.0.1", 11081))
        try:
            received, _ = datagram.recvfrom(65535)
        except socket.timeout:
            pass
        else:
            raise AssertionError(f"service-scoped credential exposed UDP: {received!r}")

    revoke_from_home(29081, home_one_credential, expect_ok=False)
    revoke_from_home(19081, home_one_credential)
    revoke_from_home(19081, home_one_credential)
    wait_scoped_catalog({"home-2": {"tcp-echo"}})
    expect_mapping_unavailable_without_cross_home_fallback(11080)

    revoke_from_home(29081, home_two_credential)
    wait_scoped_catalog({})
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
    time.sleep(3)
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
credential_id = check_home_issued_enrollment()
state = check_private_key_password_rotation()
check_udp()
check_multi_home_business_routing()
check_home_lifecycle_is_isolated()
check_embedded_spa()
check_duplicate_travel_login_is_rejected()
check_tls_policy_and_slow_loris_deadline()
failed_relay, replacement_relay = check_tcp_relay_failover()
check_scoped_authorization_and_home_revocation(failed_relay, credential_id)
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
    "home-embedded-issuer-ui",
    "home-dual-ca-issuance",
    "home-private-key-password-rotation",
    "travel-private-key-password-rotation",
    "password-rotation-survives-restart",
    "default-one-year-validity",
    "global-super-authorization",
    "home-scoped-authorization",
    "service-scoped-authorization",
    "catalog-filtered-by-signed-scope",
    "cross-home-revocation-rejected",
    "home-originated-live-revocation",
    "live-revocation-closes-flows-on-both-homes",
    "revoked-certificate-rejected-by-both-relays",
    "revocation-without-process-restart",
    "home-revocation-idempotent",
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
