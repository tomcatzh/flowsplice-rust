#!/usr/bin/env python3
"""Encrypted PTY checks reuse only a disposable business enrollment fixture."""
import json
import importlib.util
import tomllib
import os
from pathlib import Path
import subprocess
import shutil
import time


def execute(run, business_home, alpha, root, scope):
    # Import helpers from the caller without creating a second topology.
    command = run.command
    require = run.require
    wait = run.wait
    issuer = run.issuer
    travel_id = "pty-second-" + run.token
    args = ["enroll", run.relay, alpha, root, "/business/pty-second", "/business/password.txt", travel_id]
    enrollment = run.start("pty-enroll", "flowsplice-business-probe", args)
    pending = run.pending("/api/enrollment/pending", "travel_id", travel_id)
    wait(lambda: pending["verification_code"] in run.logs(enrollment), "second PTY verification code")
    issuer.request(19084, "POST", "/api/enrollment/approve", {
        "request_id": pending["request_id"], "scope": scope, "valid_days": 365, "password": run.password,
    })
    require("business-enrollment-installed" in run.finish(enrollment), "second PTY enrollment incomplete")
    secondary_setup = run.start("secondary-setup", "flowsplice-homeagent", [
        "init", "--server", run.server, "--bootstrap-config", "/config/home-bootstrap.toml",
        "--business-services", "/business/services.json", "--install-dir", "/business/home-secondary",
    ], setup=True)
    secondary_home = run.directory / "home-secondary"
    wait(lambda: (secondary_home / "home-bootstrap.json").exists(), "secondary Home bootstrap")
    secondary_id = json.loads((secondary_home / "home-bootstrap.json").read_text())["home_id"]
    require(secondary_id != scope["home_id"], "secondary Home identity was reused")
    pending = run.pending("/api/home-enrollment/pending", "home_id", secondary_id)
    wait(lambda: pending["verification_code"] in run.logs(secondary_setup), "secondary Home verification code")
    issuer.request(19084, "POST", "/api/home-enrollment/approve", {
        "request_id": pending["request_id"], "profile": "serving_only", "valid_days": 365,
        "services": ["svc-alpha"], "password": run.password,
    })
    run.finish(secondary_setup)
    secondary_descriptor = "/business/home-secondary/business-" + b"svc-alpha".hex() + ".json"
    secondary_scope = dict(scope, home_id=secondary_id)
    secondary_travel = "pty-other-home-" + run.token
    enrollment = run.start("secondary-travel-enroll", "flowsplice-business-probe", [
        "enroll", run.relay, secondary_descriptor, root, "/business/pty-other-home",
        "/business/password.txt", secondary_travel,
    ])
    pending = run.pending("/api/enrollment/pending", "travel_id", secondary_travel)
    wait(lambda: pending["verification_code"] in run.logs(enrollment), "secondary Travel verification code")
    issuer.request(19084, "POST", "/api/enrollment/approve", {
        "request_id": pending["request_id"], "scope": secondary_scope, "valid_days": 365, "password": run.password,
    })
    require("business-enrollment-installed" in run.finish(enrollment), "secondary PTY enrollment incomplete")
    (run.directory / "pty-home-secondary.toml").write_text('\n'.join([
        'home_runtime = "/business/home-secondary/home-runtime.toml"', '[[domains]]',
        'service_id = "svc-alpha"', '[domains.tmux]', 'binary = "/usr/bin/tmux"',
        'socket = "/tmp/fs-pty/alpha/tmux.sock"', 'shell = "/bin/sh"', 'working_directory = "/business"',
    ]) + '\n')
    secondary_container = run.start("secondary-pty-home", "/bin/sh", ["-c", "trap 'exit 0' TERM INT; while :; do sleep 1; done"])
    command(["docker", "exec", "-d", secondary_container, "/bin/sh", "-c",
             "exec /usr/local/bin/flowsplice-pty-home --config /business/pty-home-secondary.toml >> /business/pty-home-secondary.log 2>&1"])
    command(["docker", "stop", business_home])
    config = ['home_runtime = "/business/home/home-runtime.toml"']
    for name in ("alpha", "beta"):
        config += ['[[domains]]', f'service_id = "svc-{name}"', '[domains.tmux]',
                   'binary = "/usr/bin/tmux"', f'socket = "/tmp/fs-pty/{name}/tmux.sock"',
                   'shell = "/bin/sh"', 'working_directory = "/business"']
    (run.directory / "pty-home.toml").write_text("\n".join(config) + "\n")
    # A persistent test-owned PID 1 lets us kill/restart only Home. Restarting the
    # whole container would deliberately kill tmux and could not test keepalive.
    container = run.start("pty-home", "/bin/sh", ["-c", "trap 'exit 0' TERM INT; while :; do sleep 1; done"])
    def home_start():
        command(["docker", "exec", "-d", container, "/bin/sh", "-c",
                 "echo $$ > /business/pty-home.pid; exec /usr/local/bin/flowsplice-pty-home --config /business/pty-home.toml >> /business/pty-home.log 2>&1"])
        wait(lambda: (run.directory / "pty-home.pid").exists(), "PTY Home process PID")
    def home_stop(signal):
        pid = (run.directory / "pty-home.pid").read_text().strip()
        require(pid.isdecimal() and int(pid) > 1, "invalid isolated PTY Home PID")
        command(["docker", "exec", container, "/bin/kill", "-" + signal, pid])
        wait(lambda: command(["docker", "exec", container, "/bin/kill", "-0", pid], success=False).returncode != 0,
             "PTY Home process exit")
        (run.directory / "pty-home.pid").unlink()
    def probe(label, arguments):
        result = run.finish(run.start(label, "flowsplice-pty-probe", arguments))
        require("encrypted-pty" in result or arguments[0] == "empty", "PTY checkpoint missing")
    home_start()
    base = ["/business/travel/travelagent.toml", "/business/password.txt", root, alpha]
    try:
        probe("pty-exercise", ["exercise", base[0], "/business/pty-second/travelagent.toml", *base[1:]])
        probe("pty-multi-home", ["multi-home", base[0], "/business/pty-other-home/travelagent.toml", *base[1:], secondary_descriptor])
        run.passed.append("pty-concurrent-distinct-homes")
        probe("pty-seed", ["seed", *base, "/business/pty-session.json"])
        home_stop("TERM")
        home_start()
        probe("pty-graceful-resume", ["resume", *base, "/business/pty-session.json"])
        home_stop("KILL")
        home_start()
        probe("pty-crash-resume", ["resume", *base, "/business/pty-session.json"])
        # The fixture owns this tmux server; production exposes no remove operation.
        command(["docker", "exec", container, "/usr/bin/tmux", "-S", "/tmp/fs-pty/alpha/tmux.sock", "kill-server"])
        probe("pty-tmux-ended", ["empty", *base, "/business/pty-session.json"])
        class_spec = importlib.util.spec_from_file_location("class_acceptance", Path(__file__).with_name("check-service-class.py"))
        class_acceptance = importlib.util.module_from_spec(class_spec)
        class_spec.loader.exec_module(class_acceptance)
        service_class = class_acceptance.execute(run, alpha, root, container, secondary_container, scope, secondary_scope)
        export = os.environ.get("FLOWSPLICE_PTY_NATIVE_FIXTURE_EXPORT")
        if export:
            destination = Path(export)
            helper = Path(__file__).resolve().parents[2] / "scripts/private-travel-trust.py"
            command(["python3", str(helper), "check-path", "--path", str(destination)])
            destination.mkdir(mode=0o700, parents=True, exist_ok=False)
            for source, name in [(root, "deployment-root.pub"), ("/business/service-class.json", "service-class.json"), ("/business/password.txt", "password.txt")]:
                shutil.copyfile(run.directory / source.removeprefix("/business/"), destination / name)
                (destination / name).chmod(0o600)
            native_targets = []
            for home_scope, folder in [(scope, "home"), (secondary_scope, "home-secondary")]:
                runtime = tomllib.loads((run.directory / folder / "home-runtime.toml").read_text())
                service = next(s for s in runtime["services"] if s["id"] == home_scope["service_id"])
                name = runtime["alias"]
                if len(runtime["services"]) > 1:
                    name += " · " + service["alias"]
                native_targets.append({"id": json.dumps([home_scope["home_id"], home_scope["service_id"]], separators=(",", ":")), "home_id": home_scope["home_id"], "service_id": home_scope["service_id"], "name": name})
            (destination / "fixture.json").write_text(json.dumps({"project":run.project,"image":run.image,"pty_home_container":container,"secondary_pty_home_container":secondary_container,"secondary_scope":secondary_scope,"scope":scope,"service_class":service_class,"native_targets":native_targets,"business_directory":str(run.directory),"issuer_port":19084}))
            print(json.dumps({"checkpoint":"pty-native-fixture-ready","directory":str(destination)}), flush=True)
            deadline = time.monotonic() + 7200
            while not (destination / "native-complete.json").exists():
                require(time.monotonic() < deadline, "private native acceptance deadline exceeded")
                time.sleep(0.5)
            require(json.loads((destination / "native-complete.json").read_text()).get("all_platforms_passed") is True,
                    "private native acceptance did not pass")
            run.passed.append("private-native-ui-e2e")
        # Authorization loss removes access to the running shell, never the shell.
        command(["docker", "exec", container, "/usr/bin/tmux", "-S", "/tmp/fs-pty/alpha/tmux.sock", "kill-server"], success=False)
        access = run.start("pty-revocation", "flowsplice-pty-probe", ["access-end", *base, "/business/pty-access.json"])
        wait(lambda: "encrypted-pty-access-ready" in run.logs(access), "active PTY before revocation")
        binding = json.loads((run.directory / "travel/approved-business-binding.json").read_text())
        credential = json.loads(bytes.fromhex(binding["response"]["response"]["signed_credential"]["payload_hex"]))
        issuer.request(19084, "POST", "/api/revoke", {
            "credential_id": credential["credential_id"], "reason": "Disposable PTY revocation acceptance", "password": run.password,
        })
        require("encrypted-pty-access-ended" in run.finish(access, timeout=240), "revoked active terminal stayed connected")
        survivor = ["/business/pty-second/travelagent.toml", *base[1:]]
        probe("pty-revocation-survivor", ["resume", *survivor, "/business/pty-access.json"])
        run.passed.append("pty-active-revocation-preserves-shell")
        command(["docker", "exec", container, "/usr/bin/tmux", "-S", "/tmp/fs-pty/alpha/tmux.sock", "kill-server"])
        expiring_id = "pty-expiring-" + run.token
        enrollment = run.start("pty-expiring-enroll", "flowsplice-business-probe", [
            "enroll", run.relay, alpha, root, "/business/pty-expiring", "/business/password.txt", expiring_id,
        ])
        pending = run.pending("/api/enrollment/pending", "travel_id", expiring_id)
        wait(lambda: pending["verification_code"] in run.logs(enrollment), "expiring PTY verification code")
        issuer.request(19084, "POST", "/api/enrollment/approve", {
            "request_id": pending["request_id"], "scope": scope, "valid_minutes": 1, "password": run.password,
        })
        require("business-enrollment-installed" in run.finish(enrollment), "expiring PTY enrollment incomplete")
        access = run.start("pty-expiry", "flowsplice-pty-probe", [
            "access-end", "/business/pty-expiring/travelagent.toml", *base[1:], "/business/pty-access.json",
        ])
        require("encrypted-pty-access-ended" in run.finish(access, timeout=240), "expired active terminal stayed connected")
        probe("pty-expiry-survivor", ["resume", *survivor, "/business/pty-access.json"])
        run.passed.append("pty-active-expiry-preserves-shell")
        run.passed += ["encrypted-pty-operations", "pty-home-graceful-restart", "pty-home-crash-restart", "pty-tmux-natural-state"]
        print(json.dumps({"checkpoint":"encrypted-pty-e2e-complete","logs":str(run.evidence)}))
    finally:
        for name in ("pty-home.log", "pty-home-secondary.log"):
            log = run.directory / name
            if log.exists():
                destination = run.evidence / name
                destination.write_text(log.read_text().replace(run.password,"[REDACTED PASSWORD]"))
                destination.chmod(0o600)
