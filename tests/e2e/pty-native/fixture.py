"""Private native acceptance helpers, restricted to the exported disposable topology."""
import importlib.util
import json
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
        container = self.data["pty_home_container"]
        if not container.startswith(project + "-business-") or not container.endswith("-pty-home"):
            raise RuntimeError("Unexpected PTY Home container")
        info = json.loads(command(["docker", "inspect", container]).stdout)[0]
        if not info["State"]["Running"] or info["Config"]["Image"] != self.data["image"]:
            raise RuntimeError("Disposable PTY Home is unavailable")
        self.descriptor = json.loads((self.directory / "business.json").read_text())
        self.password = (self.directory / "password.txt").read_text().rstrip("\r\n")

    def approve_notice(self, notice):
        # The code must come from the actual rendered native UI, not the inbox itself.
        if "等待批准，校验码：" not in notice:
            return None
        code = notice.split("等待批准，校验码：", 1)[1].strip()
        pending = issuer.request(19084, "GET", "/api/enrollment/pending?page_size=100")["items"]
        matches = [item for item in pending if item["verification_code"] == code]
        if not matches:
            return None  # The UI displays its code before the request reaches Home.
        if len(matches) != 1:
            raise RuntimeError("Rendered verification code has no unique pending request")
        item = matches[0]
        uuid.UUID(item["travel_id"].removeprefix("pty-"))
        scope = self.data["scope"]
        business = item.get("business") or {}
        grant = json.loads(bytes.fromhex(self.descriptor["grant"]["payload_hex"]))
        service = next(s for s in grant["services"] if s["service_id"] == self.descriptor["service_id"])
        if (business.get("home_id") != scope["home_id"]
                or business.get("approving_home_id") != self.descriptor["approving_home_id"]
                or business.get("service") != service):
            raise RuntimeError("Native request differs from the private fixture business")
        issuer.request(19084, "POST", "/api/enrollment/approve", {
            "request_id": item["request_id"], "scope": scope,
            "valid_days": 365, "password": self.password,
        })
        return {"request_id": item["request_id"], "travel_id": item["travel_id"], "rendered_code_matched": True}

    def clear_test_sessions(self):
        # Only fixture cleanup; the product has no remove operation.
        subprocess.run(["docker", "exec", self.data["pty_home_container"], "/usr/bin/tmux",
                        "-S", "/tmp/fs-pty/alpha/tmux.sock", "kill-server"],
                       check=False, capture_output=True, timeout=15)
