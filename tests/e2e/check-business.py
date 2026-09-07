#!/usr/bin/env python3
"""Directed business acceptance against an already-running disposable Compose topology."""
import hashlib
import http.client
import importlib.util
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import time
import tomllib
import uuid

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("home_issuer_client", HERE / "home-issuer-client.py")
issuer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(issuer)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def command(args, timeout=120, success=True):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout, check=False)
    if success and result.returncode:
        raise RuntimeError(f"subprocess failed: {args[0]} exited {result.returncode}")
    return result


def wait(predicate, label):
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.25)
    raise RuntimeError("timed out: " + label)


def decode(signed):
    return json.loads(bytes.fromhex(signed["payload_hex"]))


def hashes(directory):
    return {str(p.relative_to(directory)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in directory.rglob("*") if p.is_file() and p.suffix in (".key", ".crt")}


class Run:
    def __init__(self):
        self.project = os.environ["COMPOSE_PROJECT_NAME"]
        self.image = os.environ["FLOWSPLICE_E2E_IMAGE"]
        self.uid = os.environ.get("FLOWSPLICE_E2E_UID", "0")
        self.gid = os.environ.get("FLOWSPLICE_E2E_GID", "0")
        self.token = uuid.uuid4().hex[:12]
        self.checkpoint = "topology-and-inputs"
        self.evidence = Path("/tmp") / (self.project + "-business-" + self.token + "-logs")
        self.evidence.mkdir(mode=0o700)
        self.directory = HERE / "generated" / ("business-" + self.token)
        self.directory.mkdir(mode=0o700)
        self.containers = []
        self.passed = []
        self.network = self.inspect(self.container("server"))["NetworkSettings"]["Networks"]
        require(len(self.network) == 1, "expected one isolated Compose network")
        self.network = next(iter(self.network))
        self.server = self.ip("server")
        self.relay = self.ip("relay2") + ":8443"
        self.password = (HERE / "generated/offline/test-password.txt").read_text().rstrip("\r\n")
        shutil.copyfile(HERE / "generated/offline/test-password.txt", self.directory / "password.txt")
        (self.directory / "password.txt").chmod(0o600)

    def container(self, service):
        result = command(["docker", "compose", "-f", str(HERE / "compose.yaml"), "ps", "-q", service]).stdout.strip()
        require(bool(result), "missing disposable service: " + service)
        return result

    def inspect(self, name):
        return json.loads(command(["docker", "inspect", name]).stdout)[0]

    def ip(self, service):
        return self.inspect(self.container(service))["NetworkSettings"]["Networks"][self.network]["IPAddress"]

    def start(self, label, binary, args, setup=False):
        self.checkpoint = "start-" + label
        name = self.project + "-business-" + self.token + "-" + label
        self.containers.append(name)
        argv = ["docker", "run", "-d", "--pull=never", "--name", name, "--network", self.network,
                "--user", self.uid + ":" + self.gid, "--mount", f"type=bind,source={self.directory},target=/business"]
        if setup:
            for folder in ("config", "certs"):
                argv += ["--mount", f"type=bind,source={HERE / 'generated' / folder},target=/{folder},readonly"]
        argv += ["--entrypoint", (binary if binary.startswith("/") else "/usr/local/bin/" + binary), self.image, *args]
        command(argv)
        return name

    def logs(self, name):
        result = command(["docker", "logs", name])
        return result.stdout + result.stderr

    def finish(self, name, success=True, timeout=120):
        self.checkpoint = "wait-" + name.rsplit("-", 1)[-1]
        result = command(["docker", "wait", name], timeout=timeout)
        code = int(result.stdout.strip())
        require((code == 0) == success, "unexpected exit for " + name.rsplit("-", 1)[-1])
        return self.logs(name)

    def probe(self, label, args, success=True):
        return self.finish(self.start(label, "flowsplice-business-probe", args), success)

    def pending(self, path, field, value):
        self.checkpoint = "pending-" + field
        return wait(lambda: next((item for item in issuer.request(19084, "GET", path)["items"]
                                  if item[field] == value), None), "pending " + field)

    def reject(self, path, body):
        self.checkpoint = "reject-" + path.rsplit("/", 1)[-1]
        # Require an actual client-error HTTP response; no exception is treated as rejection.
        connection = http.client.HTTPConnection("127.0.0.1", 19084, timeout=20)
        try:
            connection.request("POST", path, body=json.dumps(body).encode(), headers={
                "Authorization": "Bearer " + issuer.TOKEN,
                "Accept": "application/json", "Content-Type": "application/json",
            })
            response = connection.getresponse()
            response.read()
            require(400 <= response.status < 500,
                    f"forbidden approval returned HTTP {response.status}, expected client rejection")
        finally:
            connection.close()

    def retain_logs(self, name):
        result = command(["docker", "logs", name], timeout=30, success=False)
        text = result.stdout + result.stderr
        if getattr(self, "password", None):
            text = text.replace(self.password, "[REDACTED PASSWORD]")
        text = re.sub(r"-----BEGIN [^-]*PRIVATE KEY-----.*?-----END [^-]*PRIVATE KEY-----",
                      "[REDACTED PRIVATE KEY]", text, flags=re.DOTALL)
        text = re.sub(r'(\"?(?:password|private_key|retrieval_token_hex)\"?\s*[:=]\s*)\"[^\"]*\"',
                      r'\1"[REDACTED]"', text, flags=re.IGNORECASE)
        destination = self.evidence / (name + ".log")
        destination.write_text(text)
        destination.chmod(0o600)

    def cleanup(self):
        for name in reversed(self.containers):
            try:
                self.retain_logs(name)
            except (OSError, subprocess.TimeoutExpired):
                print(json.dumps({"checkpoint": "business-log-retention-failed", "container": name}), file=sys.stderr)
            try:
                command(["docker", "rm", "-f", name], timeout=30, success=False)
            except (OSError, subprocess.TimeoutExpired):
                print(json.dumps({"checkpoint": "business-cleanup-failed", "container": name}), file=sys.stderr)
        if self.directory is not None:
            shutil.rmtree(self.directory, ignore_errors=True)

    def execute(self):
        services = [{"service_id": value, "protocol": "tcp", "application_protocol": "flowsplice.pty.v1",
                     "capabilities": ["read", "write"]} for value in ("svc-alpha", "svc-beta", "svc-denied")]
        (self.directory / "services.json").write_text(json.dumps(services))
        setup = self.start("setup", "flowsplice-homeagent", ["init", "--server", self.server,
                           "--bootstrap-config", "/config/home-bootstrap.toml", "--business-services",
                           "/business/services.json", "--install-dir", "/business/home"], setup=True)
        home = self.directory / "home"
        wait(lambda: (home / "home-bootstrap.json").exists(), "Home bootstrap")
        home_id = json.loads((home / "home-bootstrap.json").read_text())["home_id"]
        pending = self.pending("/api/home-enrollment/pending", "home_id", home_id)
        require(pending["requested_services"] == services, "Home requested service set changed")
        wait(lambda: pending["verification_code"] in self.logs(setup), "Home verification code")
        body = {"request_id": pending["request_id"], "profile": "serving_only", "valid_days": 365,
                "services": ["svc-alpha", "svc-beta"], "password": self.password}
        for change in ({"profile": "global_issuer"}, {"services": ["unrequested"]}, {"password": "incorrect-test-password"}):
            self.reject("/api/home-enrollment/approve", body | change)
        issuer.request(19084, "POST", "/api/home-enrollment/approve", body)
        self.finish(setup)
        config = tomllib.loads((home / "home-runtime.toml").read_text())
        require("ui_listen" not in config and "issuer" not in config, "Home acquired issuer/UI configuration")
        require({s["id"] for s in config["services"]} == {"svc-alpha", "svc-beta"}, "Home approved subset changed")
        require(all(s["protocol"] == "tcp" and s["target"] == "in-process" for s in config["services"]), "Home targets not in-process")
        alpha = "/business/home/business-" + b"svc-alpha".hex() + ".json"
        beta = "/business/home/business-" + b"svc-beta".hex() + ".json"
        descriptor = json.loads((self.directory / alpha.removeprefix("/business/")).read_text())
        endpoint = decode(descriptor["endpoint"])
        grant = decode(descriptor["grant"])
        require(not endpoint.get("delegated_travel_authorities") and not endpoint.get("issuer_bundle_sha256"), "Home received issuer powers")
        require(not any("issuer" in str(p.relative_to(home)) or "authority" in p.name or p.name.endswith("ca.key") for p in home.rglob("*") if p.is_file()), "Home received issuer files")
        require(all((p.stat().st_mode & 0o077) == 0 for p in home.glob("business-*.json")), "descriptor permissions too wide")
        business_home = self.start("home", "flowsplice-socket-probe", ["home", "/business/home/home-runtime.toml"])
        self.passed.append("serving-only-subset-provisioning")
        root = "/business/home/cert/deployment-root.pub"
        install = "/business/travel"
        travel_id = "business-travel-" + self.token
        args = ["enroll", self.relay, alpha, root, install, "/business/password.txt", travel_id]
        enrollment = self.start("enroll", "flowsplice-business-probe", args)
        travel = self.directory / "travel"
        wait(lambda: (travel / "bootstrap-enrollment.json").exists(), "Travel bootstrap")
        shutil.copyfile(travel / "bootstrap-enrollment.json", self.directory / "bootstrap-backup.json")
        pending = self.pending("/api/enrollment/pending", "travel_id", travel_id)
        require(pending["business"]["home_id"] == home_id and pending["business"]["service"]["service_id"] == "svc-alpha", "Travel intent changed")
        wait(lambda: pending["verification_code"] in self.logs(enrollment), "Travel verification code")
        scope = {"kind": "service", "home_id": home_id, "service_id": "svc-alpha", "protocol": "tcp"}
        body = {"request_id": pending["request_id"], "scope": scope, "valid_days": 365, "password": self.password}
        for change in ({"scope": {"kind": "global"}}, {"scope": {"kind": "home", "home_id": home_id}},
                       {"scope": scope | {"service_id": "svc-beta"}}, {"password": "incorrect-test-password"}):
            self.reject("/api/enrollment/approve", body | change)
        issuer.request(19084, "POST", "/api/enrollment/approve", body)
        require("business-enrollment-installed" in self.finish(enrollment), "missing completed enrollment checkpoint")
        marker = json.loads((travel / "approved-business-binding.json").read_text())
        credential = decode(marker["response"]["response"]["signed_credential"])
        require(credential["scope"] == scope and credential["not_after_unix_secs"] <= grant["not_after_unix_secs"], "credential scope/expiry changed")
        require(not (travel / "bootstrap-enrollment.json").exists(), "bootstrap journal survived completion")
        config_path = travel / "travelagent.toml"
        travel_config = tomllib.loads(config_path.read_text())
        require(travel_config["homes"] == [{"id": home_id}] and home_id != descriptor["approving_home_id"], "Travel selected wrong Home")
        require(not travel_config.get("mappings"), "Travel created local mappings")
        cert_hashes = hashes(travel)
        require(bool(cert_hashes), "Travel installed no certificates")
        config_container = "/business/travel/travelagent.toml"
        travel_args = ["travel", config_container, "/business/password.txt", root, alpha]
        self.probe("traffic", travel_args)
        # A persisted issuer checkpoint is mandatory; pending disappearance is not ACK evidence.
        issuer_container = self.container("homeagent")
        wait(lambda: any("remote_enrollment_installation_confirmed" in line and pending["request_id"] in line
                         and credential["credential_id"] in line for line in self.logs(issuer_container).splitlines()), "issuing Home installation ACK")
        self.passed.append("directed-travel-and-issuer-ack")
        self.probe("resume-fixture", ["resume-fixture", install, "/business/bootstrap-backup.json"])
        self.probe("incomplete", ["check-only", config_container, root, alpha], success=False)
        self.probe("resume", args)
        require(hashes(travel) == cert_hashes, "resume changed certificates or keys")
        resumed = json.loads((travel / "approved-business-binding.json").read_text())
        require(decode(resumed["response"]["response"]["signed_credential"])["credential_id"] == credential["credential_id"], "resume reissued credential")
        self.probe("resumed-traffic", travel_args)
        self.probe("wrong-descriptor", ["check-only", config_container, root, beta], success=False)
        original = config_path.read_bytes()
        try:
            config_path.write_bytes(original + b'\n# altered installation configuration\n')
            self.probe("tampered-config", ["check-only", config_container, root, alpha], success=False)
        finally:
            config_path.write_bytes(original)
        command(["docker", "restart", business_home])
        self.probe("home-restarted", travel_args)
        self.passed += ["idempotent-install-resume", "descriptor-config-binding", "business-home-restart"]
        if os.environ.get("FLOWSPLICE_PTY_E2E", "0") == "1":
            pty_spec = importlib.util.spec_from_file_location("pty_acceptance", HERE / "check-pty.py")
            pty = importlib.util.module_from_spec(pty_spec)
            pty_spec.loader.exec_module(pty)
            self.command, self.require, self.wait, self.issuer = command, require, wait, issuer
            pty.execute(self, business_home, alpha, root, scope)
        print(json.dumps({"checkpoint": "business-e2e-complete", "passed": self.passed,
                          "logs": str(self.evidence)}))


def main():
    run = Run.__new__(Run)
    run.containers = []
    run.directory = None
    try:
        run.__init__()
        run.execute()
        return 0
    except Exception as error:
        # Do not expose subprocess output, passwords, private keys, or issuer error bodies.
        safe_error = type(error).__name__
        # Locally generated assertions are safe; HTTP helper RuntimeError bodies are not.
        if isinstance(error, RuntimeError) and error.__traceback__ is not None:
            trace = error.__traceback__
            while trace.tb_next is not None:
                trace = trace.tb_next
            if Path(trace.tb_frame.f_code.co_filename).resolve() == Path(__file__).resolve():
                safe_error = str(error)
        print(json.dumps({"checkpoint": "business-e2e-failed",
                          "stage": getattr(run, "checkpoint", "initialization"),
                          "error": safe_error, "logs": str(getattr(run, "evidence", "unavailable"))}),
              file=sys.stderr)
        return 1
    finally:
        if run is not None:
            run.cleanup()


if __name__ == "__main__":
    sys.exit(main())
