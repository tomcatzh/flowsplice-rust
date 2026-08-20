#!/usr/bin/env python3
import gzip
import http.client
import json
from pathlib import Path
import re
import socket
import subprocess
import time
import uuid

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


def container_json_request(service: str, port: int, path: str):
    result = subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "exec",
            "-T",
            service,
            "wget",
            "-qO-",
            f"http://127.0.0.1:{port}{path}",
        ],
        check=True,
        stdout=subprocess.PIPE,
    )
    return json.loads(result.stdout)


def container_page(service: str, port: int) -> bytes:
    return subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "exec",
            "-T",
            service,
            "wget",
            "-qO-",
            f"http://127.0.0.1:{port}/",
        ],
        check=True,
        stdout=subprocess.PIPE,
    ).stdout


def wait_ready() -> dict:
    deadline = time.monotonic() + 90
    last_error = None
    expected_relay_pins = json.loads(
        (Path(__file__).resolve().parent / "generated/state/relay-pins.json").read_text()
    )
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
                    == {"tcp-echo", "udp-echo", "home-1-only", "target-failure"}
                    and {service["id"] for service in homes["home-2"]["services"]}
                    == {"tcp-echo", "target-failure"}
                    and directory["generation"] >= 1
                    and {
                        relay["id"]: relay["management_spki_sha256"]
                        for relay in directory["relays"]
                    }
                    == expected_relay_pins
                ):
                    return state
        except Exception as error:  # startup polling deliberately records all transport failures
            last_error = error
        time.sleep(1)
    raise AssertionError(f"Travel Agent did not become ready: {last_error}")


def read_control_high_water() -> dict:
    generated = Path(__file__).resolve().parent / "generated"
    state_store = generated / "state/travel-state.redb"
    legacy_state = generated / "travel/control-trust-state.json"
    assert state_store.is_file() and state_store.stat().st_size > 0, state_store
    assert not legacy_state.exists(), "legacy control-trust-state.json was not migrated"
    status, _, body = http_get("/api/status", {"Accept": "application/json"})
    assert status == 200, (status, body)
    state = json.loads(body)
    assert state["catalog_generation"] >= 1, state
    assert state["relay_directory_generation"] >= 1, state
    return state


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


def check_target_failures_are_home_isolated() -> None:
    expect_mapping_unavailable_without_cross_home_fallback(11084)
    expect_mapping_unavailable_without_cross_home_fallback(11085)


def check_home_lifecycle_is_isolated() -> None:
    with (
        socket.create_connection(("127.0.0.1", 11080), timeout=5) as home_one,
        socket.create_connection(("127.0.0.1", 11082), timeout=5) as home_two,
    ):
        home_one.settimeout(15)
        home_two.settimeout(15)
        home_one_connection = exchange_line(home_one, b"before-home-two-offline")
        exchange_line(home_two, b"before-home-two-offline", b"home-2")
        subprocess.run(
            ["docker", "compose", "-f", str(COMPOSE_FILE), "stop", "homeagent2"],
            check=True,
        )
        wait_catalog_homes({"home-1"})
        assert (
            exchange_line(home_one, b"home-one-survives-home-two-offline")
            == home_one_connection
        )
        wait_local_flow_closed(home_two)
    expect_mapping_unavailable_without_cross_home_fallback(11082)
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d", "homeagent2"],
        check=True,
    )
    wait_catalog_homes({"home-1", "home-2"})
    with socket.create_connection(("127.0.0.1", 11082), timeout=5) as home_two:
        home_two.settimeout(15)
        exchange_line(home_two, b"home-two-returned", b"home-2")


def check_serving_only_home2_profile() -> None:
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "stop", "homeagent2"],
        check=True,
    )
    subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "up",
            "-d",
            "homeagent2serving",
        ],
        check=True,
    )
    wait_catalog_homes({"home-1", "home-2"})
    with socket.create_connection(("127.0.0.1", 11082), timeout=5) as home_two:
        home_two.settimeout(15)
        exchange_line(home_two, b"home-two-serving-only", b"home-2")
    time.sleep(7)
    statistics = container_json_request(
        "homeagent2serving", 9081, "/api/statistics?period=day"
    )
    assert statistics["period"] == "day", statistics
    assert statistics["points"], statistics
    page = subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "exec",
            "-T",
            "homeagent2serving",
            "wget",
            "-qO-",
            "http://127.0.0.1:9081/",
        ],
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    assert b"serving-only" in page
    issue = subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "exec",
            "-T",
            "homeagent2serving",
            "wget",
            "-qO-",
            "http://127.0.0.1:9081/api/issue",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert issue.returncode != 0, issue.stdout
    subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "stop",
            "homeagent2serving",
        ],
        check=True,
    )
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d", "homeagent2"],
        check=True,
    )
    wait_catalog_homes({"home-1", "home-2"})


def check_expired_snapshot_bootstraps_through_learned_relay() -> None:
    statistics = travel_request("GET", "/api/statistics?period=day")
    learned = {row["relay_id"]: row for row in statistics["relay_discovery"]}
    assert learned["relay-1"]["configured_seed"] is True, learned
    assert learned["relay-2"]["configured_seed"] is False, learned
    assert learned["relay-2"]["learned"] is True, learned
    relay2_success_before = learned["relay-2"]["last_success_unix_secs"]

    subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "stop",
            "travelagent",
            "relay1",
        ],
        check=True,
    )
    # The signed snapshot has a 15-second lifetime. Waiting beyond it proves that
    # restart reachability comes from redb Relay history, not the cached snapshot.
    time.sleep(18)
    subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "up",
            "-d",
            "--no-deps",
            "travelagent",
        ],
        check=True,
    )
    relay1_container = subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "ps",
            "-aq",
            "relay1",
        ],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()
    assert relay1_container
    relay1_running = subprocess.run(
        ["docker", "inspect", "-f", "{{.State.Running}}", relay1_container],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()
    assert relay1_running == "false", relay1_running
    wait_ready()
    refreshed = travel_request("GET", "/api/statistics?period=day")
    refreshed_relays = {
        row["relay_id"]: row for row in refreshed["relay_discovery"]
    }
    relay2_success_after = refreshed_relays["relay-2"]["last_success_unix_secs"]
    assert relay2_success_after is not None
    assert relay2_success_before is None or relay2_success_after > relay2_success_before
    with socket.create_connection(("127.0.0.1", 11080), timeout=5) as stream:
        stream.settimeout(15)
        exchange_line(stream, b"fresh-directory-via-learned-relay")

    subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "up",
            "-d",
            "relay1",
        ],
        check=True,
    )
    wait_ready()


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


def check_tcp_relay_failover(
    port: int = 11080, expected_home: bytes | None = None
) -> tuple[str, str]:
    with socket.create_connection(("127.0.0.1", port), timeout=5) as stream:
        stream.settimeout(30)
        connection_id = exchange_line(
            stream, b"before-relay-failure", expected_home
        )
        active = wait_active_relay()
        time.sleep(3)
        assert (
            exchange_line(stream, b"after-stable-reevaluation", expected_home)
            == connection_id
        )
        assert wait_active_relay() == active
        service = {"relay-1": "relay1", "relay-2": "relay2"}[active]
        subprocess.run(
            ["docker", "compose", "-f", str(COMPOSE_FILE), "kill", "-s", "KILL", service],
            check=True,
        )

        for sequence in range(32):
            observed = exchange_line(
                stream,
                f"after-relay-failure-{sequence:04d}".encode(),
                expected_home,
            )
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
        assert b"Business statistics" in travel_asset_body
        assert b"Configured and learned Relays" in travel_asset_body
        assert b'data-page="overview"' in travel_asset_body
        assert b'data-page="statistics"' in travel_asset_body
        assert b"Statistics are read only after this page is opened" in travel_asset_body
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
        assert "此前已经签发".encode() in issuer_javascript
        assert "业务统计".encode() in issuer_javascript
        assert "交付流量与 Relay 路径".encode() in issuer_javascript
        assert b'<details class="panel issue-panel">' in issuer_javascript
        assert "手动签发".encode() in issuer_javascript
        assert "批准使用下方".encode() not in issuer_javascript
        assert "批准并远程返回".encode() in issuer_javascript
        assert "必须用签发密码确认".encode() in issuer_javascript
        assert b'class="stats-grid"' in issuer_javascript
        assert b'class="stat-card"' in issuer_javascript
        assert b'class="stats-note"' in issuer_javascript
        assert b'class="statistics-cards"' not in issuer_javascript
        assert b'data-page="overview"' in issuer_javascript
        assert b'data-page="statistics"' in issuer_javascript
        assert "统计数据只在打开本页".encode() in issuer_javascript
        assert b"window.prompt" not in issuer_javascript

    issuer_assets = {
        asset.decode()
        for asset in re.findall(
            rb'(?:src|href)="(/assets/[^"]+\.(?:js|css))"', issuer_body
        )
    }
    issuer_css = next(asset for asset in issuer_assets if asset.endswith(".css"))
    issuer_css_status, issuer_css_headers, issuer_css_body = issuer_get(
        issuer_css, {"Accept-Encoding": "gzip"}
    )
    assert issuer_css_status == 200
    assert issuer_css_headers.get("content-encoding") == "gzip"
    issuer_stylesheet = gzip.decompress(issuer_css_body)
    assert b".scope input[type=radio]" in issuer_stylesheet
    assert b"width:18px" in issuer_stylesheet
    assert b"height:18px" in issuer_stylesheet
    assert b"min-height:18px" in issuer_stylesheet
    assert b"padding:0" in issuer_stylesheet

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
    for relay_addr, relay_id in [
        ("relay1:8443", "relay-1"),
        ("relay2:8443", "relay-2"),
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
    request = json.loads((generated / "travel/enrollment-request.json").read_text())
    request["request_id"] = str(uuid.uuid4())
    request["created_at_unix_secs"] = int(time.time())
    result = issuer_request(
        port,
        "POST",
        "/api/issue",
        {
            "request": request,
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
    generated = Path(__file__).resolve().parent / "generated"
    issuer_directory = "offline-home2" if port == 29081 else "offline"
    return issuer_request(
        port,
        "POST",
        "/api/revoke",
        {
            "credential_id": credential_id,
            "reason": "E2E revocation",
            "password": (generated / issuer_directory / "test-password.txt")
            .read_text()
            .strip(),
        },
        expect_ok=expect_ok,
    )


def revoke_with_wrong_password(port: int, credential_id: str) -> dict:
    return issuer_request(
        port,
        "POST",
        "/api/revoke",
        {
            "credential_id": credential_id,
            "reason": "must not be accepted",
            "password": "wrong-flowsplice-e2e-revocation-password",
        },
        expect_ok=False,
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
    generated = Path(__file__).resolve().parent / "generated"
    response = json.loads(
        (generated / "authorization/enrollment-response.json").read_text()
    )
    assert "authority_public_key" not in response
    assert "management_ca_certificate_pem" not in response
    trust = json.loads(bytes.fromhex(response["deployment_trust"]["payload_hex"]))
    credential_payload = json.loads(
        bytes.fromhex(response["signed_credential"]["payload_hex"])
    )
    assert credential_payload["object_type"] == "flowsplice.travel_credential"
    assert credential_payload["deployment_id"] == trust["deployment_id"]
    assert credential_payload["enrollment_request_id"] == response["approval"]["request"]["request_id"]
    assert credential_payload["enrollment_nonce"] == response["approval"]["request"]["nonce"]
    assert len(credential_payload["enrollment_nonce"]) == 64
    assert credential_payload["authority_epoch"] == 1
    for name in (
        "deployment_trust_sha256",
        "enrollment_request_sha256",
        "management_ca_sha256",
        "business_ca_sha256",
        "management_certificate_sha256",
        "business_certificate_sha256",
    ):
        assert len(credential_payload[name]) == 64, name
    assert trust["management_ca_certificate_pem"].startswith(
        "-----BEGIN CERTIFICATE-----"
    )
    assert trust["business_ca_certificate_pem"].startswith(
        "-----BEGIN CERTIFICATE-----"
    )
    assert (generated / "travel/management-ca.crt").read_text() == trust[
        "management_ca_certificate_pem"
    ]
    assert (generated / "travel/business-ca.crt").read_text() == trust[
        "business_ca_certificate_pem"
    ]
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


def check_duplicate_enrollment_issue_is_idempotent(
    credential_id: str, expected_generation: int | None = None
) -> int:
    generated = Path(__file__).resolve().parent / "generated"
    before = issued_credentials(19081)
    result = issuer_request(
        19081,
        "POST",
        "/api/issue",
        {
            "request": json.loads(
                (generated / "travel/enrollment-request.json").read_text()
            ),
            "valid_days": 365,
            "scope": {"kind": "global"},
            "password": "deliberately-not-the-issuer-password",
        },
    )
    assert result["reused"] is True, result
    assert result["enrollment"]["approval"]["credential_id"] == credential_id, result
    if expected_generation is not None:
        assert result["generation"] == expected_generation, result
    after = issued_credentials(19081)
    assert after == before, (before, after)

    changed_scope = issuer_request(
        19081,
        "POST",
        "/api/issue",
        {
            "request": json.loads(
                (generated / "travel/enrollment-request.json").read_text()
            ),
            "valid_days": 365,
            "scope": {"kind": "home", "home_id": "home-1"},
            "password": "password-must-not-change-a-used-request",
        },
        expect_ok=False,
    )
    assert "already used for a different authorization" in changed_scope["error"], changed_scope
    assert issued_credentials(19081) == before
    return result["generation"]


def check_private_key_password_rotation(
    credential_id: str, issuance_generation: int
) -> dict:
    generated = Path(__file__).resolve().parent / "generated"
    old_password = (generated / "offline/test-password.txt").read_text().strip()
    new_password = "flowsplice-e2e-rotated-private-key-password"
    if new_password == old_password:
        new_password = "flowsplice-e2e-rotated-private-key-password-again"
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
    before_restart = read_control_high_water()
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "restart", "homeagent", "travelagent"],
        check=True,
    )
    wait_issuer_ready(19081)
    check_duplicate_enrollment_issue_is_idempotent(
        credential_id, issuance_generation
    )
    ready = wait_ready()
    after_restart = read_control_high_water()
    assert after_restart["catalog_generation"] >= before_restart["catalog_generation"]
    assert (
        after_restart["relay_directory_generation"]
        >= before_restart["relay_directory_generation"]
    )
    return ready


def wait_remote_enrollment_status(request_id: str, field: str, expected=True) -> dict:
    deadline = time.monotonic() + 90
    last = None
    while time.monotonic() < deadline:
        records = travel_request("GET", "/api/enrollment")
        last = next(
            (record for record in records if record["request_id"] == request_id), None
        )
        if last is not None and last[field] == expected:
            return last
        time.sleep(1)
    raise AssertionError(
        f"remote enrollment {request_id} did not reach {field}={expected}: {last}"
    )


def prepare_remote_enrollment() -> tuple[str, str]:
    generated = Path(__file__).resolve().parent / "generated"
    password = (generated / "travel/test-password.txt").read_text().strip()
    created = travel_request(
        "POST", "/api/enrollment", {"home_id": "home-1", "password": password}
    )
    request_id = created["request_id"]

    deadline = time.monotonic() + 90
    pending = None
    while time.monotonic() < deadline:
        records = issuer_request(19081, "GET", "/api/enrollment/pending")
        pending = next(
            (record for record in records if record["request_id"] == request_id), None
        )
        if pending is not None:
            break
        time.sleep(1)
    assert pending is not None and pending["approved"] is False, pending

    issuer_request(
        19081,
        "POST",
        "/api/enrollment/approve",
        {
            "request_id": request_id,
            "valid_days": 365,
            "scope": {"kind": "global"},
            "password": "wrong-flowsplice-e2e-enrollment-password",
        },
        expect_ok=False,
    )
    pending_after_wrong_password = next(
        record
        for record in issuer_request(19081, "GET", "/api/enrollment/pending")
        if record["request_id"] == request_id
    )
    assert pending_after_wrong_password["approved"] is False

    approved = issuer_request(
        19081,
        "POST",
        "/api/enrollment/approve",
        {
            "request_id": request_id,
            "valid_days": 365,
            "scope": {"kind": "global"},
            "password": (generated / "offline/test-password.txt").read_text().strip(),
        },
    )
    assert approved["enrollment"]["approval"]["request"]["request_id"] == request_id
    wait_remote_enrollment_status(request_id, "response_received")
    return request_id, password


def activate_remote_enrollment(request_id: str, password: str) -> tuple[dict, str]:
    travel_request(
        "POST",
        "/api/enrollment/install",
        {"request_id": request_id, "password": "wrong-install-password"},
        expect_ok=False,
    )
    before = wait_remote_enrollment_status(request_id, "response_received")
    assert before["restart_required"] is False, before
    installed = travel_request(
        "POST",
        "/api/enrollment/install",
        {"request_id": request_id, "password": password},
    )
    assert installed["restart_required"] is True, installed
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "restart", "travelagent"],
        check=True,
    )
    ready = wait_ready()
    with socket.create_connection(("127.0.0.1", 11080), timeout=5) as stream:
        stream.settimeout(15)
        exchange_line(stream, b"remote-enrollment-identity-active")
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        travel_pending = travel_request("GET", "/api/enrollment")
        home_pending = issuer_request(19081, "GET", "/api/enrollment/pending")
        if (
            all(record["request_id"] != request_id for record in travel_pending)
            and all(record["request_id"] != request_id for record in home_pending)
        ):
            break
        time.sleep(1)
    else:
        raise AssertionError("installed remote enrollment was not acknowledged and retired")
    return ready, installed["credential_id"]


def check_home2_remote_enrollment(password: str) -> str:
    generated = Path(__file__).resolve().parent / "generated"
    created = travel_request(
        "POST", "/api/enrollment", {"home_id": "home-2", "password": password}
    )
    request_id = created["request_id"]
    deadline = time.monotonic() + 90
    pending = None
    while time.monotonic() < deadline:
        home_one_records = issuer_request(19081, "GET", "/api/enrollment/pending")
        home_two_records = issuer_request(29081, "GET", "/api/enrollment/pending")
        assert all(record["request_id"] != request_id for record in home_one_records)
        pending = next(
            (record for record in home_two_records if record["request_id"] == request_id),
            None,
        )
        if pending is not None:
            break
        time.sleep(1)
    assert pending is not None and pending["home_id"] == "home-2", pending
    issuer_request(
        19081,
        "POST",
        "/api/enrollment/approve",
        {
            "request_id": request_id,
            "valid_days": 365,
            "scope": {"kind": "home", "home_id": "home-2"},
            "password": (generated / "offline/test-password.txt").read_text().strip(),
        },
        expect_ok=False,
    )
    issuer_request(
        29081,
        "POST",
        "/api/enrollment/approve",
        {
            "request_id": request_id,
            "valid_days": 365,
            "scope": {"kind": "home", "home_id": "home-2"},
            "password": "wrong-home-two-enrollment-password",
        },
        expect_ok=False,
    )
    approved = issuer_request(
        29081,
        "POST",
        "/api/enrollment/approve",
        {
            "request_id": request_id,
            "valid_days": 365,
            "scope": {"kind": "home", "home_id": "home-2"},
            "password": (generated / "offline-home2/test-password.txt")
            .read_text()
            .strip(),
        },
    )
    credential_id = approved["enrollment"]["approval"]["credential_id"]
    wait_remote_enrollment_status(request_id, "response_received")
    travel_request(
        "POST",
        "/api/enrollment/install",
        {"request_id": request_id, "password": password},
    )
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "restart", "travelagent"],
        check=True,
    )
    wait_scoped_catalog({"home-2": {"tcp-echo", "target-failure"}})
    with socket.create_connection(("127.0.0.1", 11082), timeout=5) as home_two:
        home_two.settimeout(15)
        exchange_line(home_two, b"remote-home-two-scope", b"home-2")
    expect_mapping_unavailable_without_cross_home_fallback(11080)

    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        if (
            all(
                record["request_id"] != request_id
                for record in travel_request("GET", "/api/enrollment")
            )
            and all(
                record["request_id"] != request_id
                for record in issuer_request(29081, "GET", "/api/enrollment/pending")
            )
        ):
            break
        time.sleep(1)
    else:
        raise AssertionError("Home-2 remote enrollment was not acknowledged and retired")

    revoke_with_wrong_password(29081, credential_id)
    active = next(
        credential
        for credential in issued_credentials(29081)
        if credential["credential_id"] == credential_id
    )
    assert active["active"] and not active["revoked"], active
    revoke_from_home(29081, credential_id)
    wait_scoped_catalog({})
    return credential_id


def check_server_failure_does_not_break_established_data_flow() -> None:
    server_config = (
        Path(__file__).resolve().parent / "generated/config/server.toml"
    ).read_text()
    assert "data_listen" not in server_config
    listener_probe = subprocess.run(
        [
            "docker",
            "compose",
            "-f",
            str(COMPOSE_FILE),
            "exec",
            "-T",
            "server",
            "sh",
            "-c",
            "nc -z -w 1 127.0.0.1 7444",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert listener_probe.returncode != 0, listener_probe.stdout
    with (
        socket.create_connection(("127.0.0.1", 11080), timeout=5) as home_one,
        socket.create_connection(("127.0.0.1", 11082), timeout=5) as home_two,
    ):
        home_one.settimeout(20)
        home_two.settimeout(20)
        home_one_connection = exchange_line(home_one, b"before-server-stop")
        home_two_connection = exchange_line(
            home_two, b"before-server-stop", b"home-2"
        )
        subprocess.run(
            ["docker", "compose", "-f", str(COMPOSE_FILE), "stop", "server"],
            check=True,
        )
        assert (
            exchange_line(home_one, b"server-stopped-direct-flow")
            == home_one_connection
        )
        assert (
            exchange_line(home_two, b"server-stopped-direct-flow", b"home-2")
            == home_two_connection
        )
        subprocess.run(
            ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d", "server"],
            check=True,
        )
        wait_ready()
        assert (
            exchange_line(home_one, b"server-returned-same-flow")
            == home_one_connection
        )
        assert (
            exchange_line(home_two, b"server-returned-same-flow", b"home-2")
            == home_two_connection
        )
    deadline = time.monotonic() + 45
    last_error = None
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", 11080), timeout=3) as fresh:
                fresh.settimeout(6)
                exchange_line(fresh, b"new-route-after-server-recovery")
                return
        except (AssertionError, ConnectionError, OSError, socket.timeout) as error:
            last_error = error
            time.sleep(1)
    raise AssertionError(f"new route did not recover after Server restart: {last_error}")


def check_statistics_pipeline() -> None:
    deadline = time.monotonic() + 45
    last = None
    while time.monotonic() < deadline:
        travel = travel_request("GET", "/api/statistics?period=day")
        home = issuer_request(19081, "GET", "/api/statistics?period=week")
        relay_one = container_json_request(
            "relay1", 9084, "/api/statistics?period=month"
        )
        relay_two = container_json_request(
            "relay2", 9084, "/api/statistics?period=year"
        )
        server = container_json_request(
            "server", 9083, "/api/statistics?period=day"
        )
        last = (travel, home, relay_one, relay_two, server)
        roles = {
            report["payload"]["reporter_role"].lower()
            for report in server["reports"]
        }
        if (
            travel["points"]
            and home["points"]
            and (relay_one["points"] or relay_two["points"])
            and {"travel", "home", "relay"}.issubset(roles)
        ):
            assert travel["period"] == "day"
            assert home["period"] == "week"
            assert relay_one["period"] == "month"
            assert relay_two["period"] == "year"
            assert travel["overview"] and travel["breakdowns"]
            assert home["overview"] and home["breakdowns"]
            assert server["overview"] and server["breakdowns"] and server["nodes"]
            assert all(
                not report["payload"]["metric_family"].startswith("server_")
                and "control" not in report["payload"]["metric_family"]
                for report in server["reports"]
            )
            for report in server["reports"]:
                role = report["payload"]["reporter_role"].lower()
                family = report["payload"]["metric_family"]
                if role == "travel":
                    assert family.startswith(("delivered_download", "carrier_", "travel_flow_"))
                elif role == "home":
                    assert family.startswith(("delivered_upload", "home_flow_", "target_", "issuer_"))
                elif role == "relay":
                    assert family.startswith(("relay_transport_", "relay_route_"))
                else:
                    raise AssertionError((role, family))
            raw_sums = {}
            for report in server["reports"]:
                role = report["payload"]["reporter_role"].lower()
                family = report["payload"]["metric_family"]
                authoritative = (
                    (role == "travel" and family.startswith(("delivered_download", "carrier_")))
                    or (
                        role == "home"
                        and family.startswith(
                            (
                                "delivered_upload",
                                "home_flow_accepted",
                                "home_flow_completed",
                                "home_flow_failed",
                                "target_",
                                "issuer_",
                            )
                        )
                    )
                    or (role == "relay" and family.startswith("relay_"))
                )
                if not authoritative:
                    continue
                raw_sums[family] = raw_sums.get(family, 0) + report["payload"]["value"]["sum"]
            assert {row["metric_family"]: row["sum"] for row in server["overview"]} == raw_sums
            target_failure_reporters = {
                report["payload"]["reporter_id"]
                for report in server["reports"]
                if report["payload"]["reporter_role"].lower() == "home"
                and report["payload"]["metric_family"] == "target_failure"
            }
            assert {"home-1", "home-2"}.issubset(target_failure_reporters)
            for service, port, title in (
                ("relay1", 9084, b"Relay statistics"),
                ("server", 9083, b"Global statistics"),
            ):
                page = container_page(service, port)
                assert title in page
                assert b"Report window" in page
                assert b"Five-minute series" in page
                assert b'id="statistics-page" class="view" hidden' in page
                assert b"activate('overview')" in page
                assert b"render();setInterval(render,30000)" not in page
                assert b"<pre>" not in page
            return
        time.sleep(1)
    raise AssertionError(f"statistics reports did not converge: {last}")


def home_delivered_upload_sum(port: int) -> int:
    statistics = issuer_request(port, "GET", "/api/statistics?period=day")
    return sum(
        point["value"]["sum"]
        for point in statistics["points"]
        if point["identity"]["metric_family"] == "delivered_upload_bytes"
        and point["identity"]["dimensions"].get("service_id") == "tcp-echo"
    )


def check_home_statistics_write_failures_are_isolated() -> None:
    for service, issuer_port, business_port, expected_home in (
        ("homeagent", 19081, 11080, None),
        ("homeagent2", 29081, 11082, b"home-2"),
    ):
        before = home_delivered_upload_sum(issuer_port)
        armed = issuer_request(
            issuer_port,
            "POST",
            "/api/test/statistics-flush-failures",
            {"failures": 1},
        )
        assert armed["remaining"] == 1, armed
        payload = f"{service}-statistics-write-failure".encode()
        with socket.create_connection(("127.0.0.1", business_port), timeout=5) as stream:
            stream.settimeout(15)
            exchange_line(stream, payload, expected_home)

        deadline = time.monotonic() + 20
        remaining = None
        while time.monotonic() < deadline:
            remaining = issuer_request(
                issuer_port, "GET", "/api/test/statistics-flush-failures"
            )["remaining"]
            if remaining == 0:
                break
            time.sleep(0.25)
        assert remaining == 0, (service, remaining)

        # Business remains available while the node is recovering its best-effort statistics
        # write path. The next successful flush must include the delta restored after failure.
        with socket.create_connection(("127.0.0.1", business_port), timeout=5) as stream:
            stream.settimeout(15)
            exchange_line(stream, b"business-survives-statistics-write-failure", expected_home)
        deadline = time.monotonic() + 20
        after = before
        while time.monotonic() < deadline:
            after = home_delivered_upload_sum(issuer_port)
            if after >= before + len(payload):
                break
            time.sleep(0.5)
        assert after >= before + len(payload), (service, before, after)

        logs = subprocess.run(
            [
                "docker",
                "compose",
                "-f",
                str(COMPOSE_FILE),
                "logs",
                "--no-color",
                service,
            ],
            check=True,
            stdout=subprocess.PIPE,
        ).stdout
        assert b"injected statistics redb write failure" in logs, service


def assert_revoked_management_certificate_rejected(relay: str) -> None:
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
        try:
            status, _, body = http_get("/api/catalog", {"Accept": "application/json"})
            if status == 200:
                catalog = json.loads(body)
                last = {
                    home["home_id"]: {service["id"] for service in home["services"]}
                    for home in catalog["homes"]
                }
                if last == expected:
                    return
        except (ConnectionError, OSError) as error:
            last = error
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
        wrong_password = revoke_with_wrong_password(19081, global_credential_id)
        assert wrong_password["error"], wrong_password
        still_active = next(
            credential
            for credential in issued_credentials(19081)
            if credential["credential_id"] == global_credential_id
        )
        assert still_active["active"] and not still_active["revoked"], still_active
        exchange_line(home_one, b"wrong-revoke-password-kept-home-one-open")
        exchange_line(
            home_two,
            b"wrong-revoke-password-kept-home-two-open",
            b"home-2",
        )
        revoke_from_home(19081, global_credential_id)
        wait_local_flow_closed(home_one)
        wait_local_flow_closed(home_two)

    generated = Path(__file__).resolve().parent / "generated"
    revoked_retry = issuer_request(
        19081,
        "POST",
        "/api/issue",
        {
            "request": json.loads(
                (generated / "travel/enrollment-request.json").read_text()
            ),
            "valid_days": 365,
            "scope": {"kind": "global"},
            "password": "password-must-not-reactivate-a-revoked-intent",
        },
        expect_ok=False,
    )
    assert "no longer active" in revoked_retry["error"], revoked_retry

    wait_scoped_catalog(
        {"home-1": {"tcp-echo"}, "home-2": {"tcp-echo", "target-failure"}}
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
    wait_scoped_catalog({"home-2": {"tcp-echo", "target-failure"}})
    expect_mapping_unavailable_without_cross_home_fallback(11080)

    revoke_from_home(29081, home_two_credential)
    wait_scoped_catalog({})
    assert_revoked_management_certificate_rejected("relay1")
    assert_revoked_management_certificate_rejected("relay2")
    assert pids == {
        service_name: container_pid(service_name)
        for service_name in ("server", "relay1", "relay2", "homeagent", "homeagent2")
    }
    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "restart", "relay2"],
        check=True,
    )
    time.sleep(3)
    assert_revoked_management_certificate_rejected("relay2")


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
            "relay-1",
        ],
        check=True,
    )


def checkpoint(name: str) -> None:
    print(json.dumps({"checkpoint": name}), flush=True)


checkpoint("initial-ready")
state = wait_ready()
checkpoint("expired-snapshot-learned-relay")
check_expired_snapshot_bootstraps_through_learned_relay()
checkpoint("home-issued-enrollment")
credential_id = check_home_issued_enrollment()
checkpoint("duplicate-enrollment-idempotency")
issuance_generation = check_duplicate_enrollment_issue_is_idempotent(credential_id)
checkpoint("private-key-password-rotation")
state = check_private_key_password_rotation(credential_id, issuance_generation)
checkpoint("prepare-remote-enrollment")
remote_request_id, remote_identity_password = prepare_remote_enrollment()
checkpoint("udp")
check_udp()
checkpoint("multi-home-business-routing")
check_multi_home_business_routing()
checkpoint("per-home-target-failures")
check_target_failures_are_home_isolated()
checkpoint("home-lifecycle-isolation")
check_home_lifecycle_is_isolated()
checkpoint("serving-only-home2")
check_serving_only_home2_profile()
checkpoint("server-outage-established-flow")
check_server_failure_does_not_break_established_data_flow()
checkpoint("embedded-spa")
check_embedded_spa()
checkpoint("duplicate-travel-login")
check_duplicate_travel_login_is_rejected()
checkpoint("tls-policy-and-slow-loris")
check_tls_policy_and_slow_loris_deadline()
checkpoint("statistics-pipeline")
check_statistics_pipeline()
checkpoint("home-statistics-write-failure-isolation")
check_home_statistics_write_failures_are_isolated()
checkpoint("home1-tcp-relay-failover")
failed_relay, replacement_relay = check_tcp_relay_failover()
subprocess.run(
    [
        "docker",
        "compose",
        "-f",
        str(COMPOSE_FILE),
        "up",
        "-d",
        {"relay-1": "relay1", "relay-2": "relay2"}[failed_relay],
    ],
    check=True,
)
wait_ready()
checkpoint("home2-tcp-relay-failover")
home2_failed_relay, _ = check_tcp_relay_failover(11082, b"home-2")
checkpoint("scoped-authorization-and-revocation")
check_scoped_authorization_and_home_revocation(home2_failed_relay, credential_id)
checkpoint("activate-remote-enrollment")
state, remote_credential_id = activate_remote_enrollment(
    remote_request_id, remote_identity_password
)
assert remote_credential_id != credential_id
checkpoint("home2-remote-enrollment")
home2_remote_credential_id = check_home2_remote_enrollment(remote_identity_password)
assert home2_remote_credential_id not in {credential_id, remote_credential_id}
checks = [
    "server-downlinked-two-relay-spki-directory",
    "two-home-catalog",
    "same-service-id-isolated-by-home",
    "logical-business-selects-exact-home",
    "wrong-home-service-fails-without-fallback",
    "target-failure-isolated-per-home",
    "one-home-offline-does-not-affect-other-home",
    "home-catalog-removal-and-return",
    "home-two-serving-only-profile",
    "home-two-serving-only-has-no-issuer-routes-or-keys",
    "home-two-state-persists-across-issuer-to-serving-only-restart",
    "same-tcp-flow-relay-failover",
    "home-two-same-tcp-flow-relay-failover",
    "same-home-target-connection",
    "server-failure-does-not-break-established-direct-flow",
    "server-failure-preserves-both-home-flows",
    "server-has-no-business-data-listener",
    "udp",
    "embedded-spa",
    "tls13-only",
    "slow-frame-deadline",
    "duplicate-travel-login-rejected-across-relays",
    "encrypted-local-travel-enrollment",
    "tampered-enrollment-trust-rejected",
    "cross-request-and-certificate-splicing-rejected",
    "independent-enrollment-nonce-bound",
    "single-file-enrollment-response-includes-ca-roots",
    "home-embedded-issuer-ui",
    "home-dual-ca-issuance",
    "duplicate-enrollment-issue-is-idempotent",
    "enrollment-request-is-single-use",
    "issuance-ledger-survives-home-restart",
    "home-private-key-password-rotation",
    "travel-private-key-password-rotation",
    "password-rotation-survives-restart",
    "deployment-root-private-key-not-mounted-in-home",
    "durable-control-high-water-survives-restart",
    "expired-snapshot-bootstrap-through-learned-relay",
    "fresh-directory-required-before-learned-relay-business",
    "default-one-year-validity",
    "global-super-authorization",
    "home-scoped-authorization",
    "service-scoped-authorization",
    "catalog-filtered-by-signed-scope",
    "cross-home-revocation-rejected",
    "home-originated-live-revocation",
    "duplicate-issuance-cannot-reactivate-revoked-grant",
    "live-revocation-closes-flows-on-both-homes",
    "revoked-certificate-rejected-by-both-relays",
    "revocation-without-process-restart",
    "home-revocation-idempotent",
    "restarted-relay-retains-revocation",
    "five-minute-local-statistics",
    "signed-statistics-upload",
    "server-statistics-idempotent-collection",
    "day-week-month-year-statistics-queries",
    "statistics-write-failure-isolated-per-home",
    "remote-enrollment-outbox-to-home-inbox",
    "remote-enrollment-wrong-approval-password-rejected",
    "remote-enrollment-wrong-install-password-rejected",
    "remote-enrollment-restart-activation",
    "remote-enrollment-installed-acknowledgement",
    "remote-enrollment-new-identity-restores-business",
    "remote-enrollment-generates-no-business-mapping",
    "home-two-remote-enrollment-isolation",
    "home-two-remote-enrollment-password-gate",
    "home-two-remote-enrollment-install-acknowledgement",
    "home-two-password-gated-revocation",
]
print(
    json.dumps(
        {
            "result": "ok",
            "travel": state["travel_id"],
            "failed_relay": failed_relay,
            "replacement_relay": replacement_relay,
            "home2_failed_relay": home2_failed_relay,
            "remote_credential_id": remote_credential_id,
            "home2_remote_credential_id": home2_remote_credential_id,
            "checks": checks,
        }
    )
)
