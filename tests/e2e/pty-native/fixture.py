"""Private native acceptance helpers, restricted to the exported disposable topology."""
import importlib.util
import json
import re
from pathlib import Path
import subprocess
import uuid

REPO = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location("issuer", REPO / "tests/e2e/home-issuer-client.py")
issuer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(issuer)


def command(args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, timeout=60, **kwargs)


class Fixture:
    def __init__(self, directory):
        self.directory = Path(directory).resolve(strict=True)
        command(["python3", str(REPO / "scripts/private-travel-trust.py"), "check-path", "--path", str(self.directory)])
        self.data = json.loads((self.directory / "fixture.json").read_text())
        project = self.data["project"]
        if not project.startswith("flowsplice-e2e-") or self.data["issuer_port"] != 19084:
            raise RuntimeError("Not a disposable native fixture")
        self.containers = [self.data["pty_home_container"]]
        if self.data.get("secondary_pty_home_container"):
            self.containers.append(self.data["secondary_pty_home_container"])
        for container in self.containers:
            if not container.startswith(project + "-business-") or not container.endswith("-pty-home"):
                raise RuntimeError("Unexpected PTY Home container")
            info = json.loads(command(["docker", "inspect", container]).stdout)[0]
            if not info["State"]["Running"] or info["Config"]["Image"] != self.data["image"]:
                raise RuntimeError("Disposable PTY Home is unavailable")
        self.service_class = None
        self.native_targets = []
        if (self.directory / "service-class.json").is_file():
            self.service_class = json.loads((self.directory / "service-class.json").read_text())
            if (set(self.service_class) != {"version", "approving_home_id", "application_protocol", "protocol"}
                    or self.service_class["version"] != 1
                    or self.service_class["application_protocol"] != "flowsplice.pty.v1"
                    or self.service_class["protocol"] != "tcp"):
                raise RuntimeError("Unexpected native service class descriptor")
            self.native_targets = self.data.get("native_targets", [])
            if len(self.native_targets) != 2:
                raise RuntimeError("Class acceptance requires exactly two explicit native targets")
            ids, home_ids = set(), set()
            for target in self.native_targets:
                if (json.loads(target["id"]) != [target["home_id"], target["service_id"]]
                        or not target["name"].strip() or target["id"] in ids):
                    raise RuntimeError("Invalid native class target")
                ids.add(target["id"]); home_ids.add(target["home_id"])
            if len(home_ids) != 2 or len(self.containers) != 2 or len({target["name"] for target in self.native_targets}) != 2:
                raise RuntimeError("Class acceptance requires two different fixture Homes")
            self.targets = self.native_targets
            self.password = (self.directory / "password.txt").read_text().rstrip("\r\n")
            return
        self.descriptor = json.loads((self.directory / "business.json").read_text())
        self.targets = [(self.descriptor, self.data["scope"])]
        if self.data.get("secondary_scope"):
            homes = json.loads((self.directory / "homes.json").read_text())["homes"]
            secondary = next(home["descriptor"] for home in homes if home["id"] == "secondary")
            self.targets.append((secondary, self.data["secondary_scope"]))
        self.password = (self.directory / "password.txt").read_text().rstrip("\r\n")

    @property
    def expected_approvals(self):
        return 1 if self.service_class else 2

    def approve_notice(self, notice, client_label=None):
        if isinstance(notice, dict):
            client_label = notice.get("client_label")
            notice = notice.get("notice")
        if not isinstance(notice, str):
            raise RuntimeError("Invalid rendered verification evidence")
        # The code must come from the actual rendered native UI, not the inbox itself.
        if "等待批准，校验码：" not in notice:
            return None
        rendered = notice.split("等待批准，校验码：", 1)[1]
        # Whitespace may wrap the code visually; never accept an inbox-only code.
        rendered = re.sub(r"\s+", "", rendered)
        pending = issuer.request(19084, "GET", "/api/enrollment/pending?page_size=100")["items"]
        matches = [item for item in pending if re.sub(r"\s+", "", item["verification_code"]) == rendered]
        if not matches:
            return None  # The UI displays its code before the request reaches Home.
        if len(matches) != 1:
            raise RuntimeError("Rendered verification code has no unique pending request")
        item = matches[0]
        uuid.UUID(item["travel_id"].removeprefix("pty-"))
        if self.service_class:
            if (not isinstance(client_label, str) or not client_label.endswith(" · PTY")
                    or len(client_label.encode()) > 64
                    or any(ord(c) < 32 or 127 <= ord(c) <= 159 or 0x202A <= ord(c) <= 0x202E or 0x2066 <= ord(c) <= 0x2069 for c in client_label)
                    or item.get("client_label") != client_label
                    or item.get("service_class") != self.service_class):
                raise RuntimeError("Native class request differs from rendered identity or fixture class")
            scope = {"kind":"service_class", "application_protocol":self.service_class["application_protocol"], "protocol":self.service_class["protocol"]}
            issuer.request(19084, "POST", "/api/enrollment/approve", {
                "request_id":item["request_id"], "scope":scope,
                "valid_days":365, "password":self.password,
            })
            return {"request_id":item["request_id"], "travel_id":item["travel_id"],
                    "rendered_code_matched":True, "client_label":client_label,
                    "service_class":self.service_class, "scope":scope}
        business = item.get("business") or {}
        targets = [(descriptor, scope) for descriptor, scope in self.targets if scope["home_id"] == business.get("home_id")]
        if len(targets) != 1:
            raise RuntimeError("Native request is outside the private fixture Homes")
        descriptor, scope = targets[0]
        grant = json.loads(bytes.fromhex(descriptor["grant"]["payload_hex"]))
        service = next(s for s in grant["services"] if s["service_id"] == descriptor["service_id"])
        if (business.get("approving_home_id") != descriptor["approving_home_id"]
                or business.get("service") != service):
            raise RuntimeError("Native request differs from the private fixture business")
        issuer.request(19084, "POST", "/api/enrollment/approve", {
            "request_id": item["request_id"], "scope": scope,
            "valid_days": 365, "password": self.password,
        })
        return {"request_id": item["request_id"], "travel_id": item["travel_id"], "rendered_code_matched": True}

    def clear_test_sessions(self):
        # Only fixture cleanup; the product has no remove operation.
        for container in self.containers:
            subprocess.run(["docker", "exec", container, "/usr/bin/tmux",
                            "-S", "/tmp/fs-pty/alpha/tmux.sock", "kill-server"],
                           check=False, capture_output=True, timeout=15)

    def delete_second_session(self):
        """External deletion test; accept one test-owned UUID session only."""
        if len(self.containers) != 2:
            raise RuntimeError("Deletion requires the isolated two-Home fixture")
        container = self.containers[1]
        base = ["docker", "exec", container, "/usr/bin/tmux", "-S", "/tmp/fs-pty/alpha/tmux.sock"]
        names = command(base + ["list-sessions", "-F", "#{session_name}"]).stdout.decode().splitlines()
        if len(names) != 1 or re.fullmatch(r"fs-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", names[0]) is None:
            raise RuntimeError("Refusing deletion outside the single fixture UUID session")
        command(base + ["kill-session", "-t", "=" + names[0]])
        return {"container":container, "session":names[0], "deleted":True}
