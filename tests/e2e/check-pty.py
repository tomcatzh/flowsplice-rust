#!/usr/bin/env python3
"""Encrypted PTY checks reuse only a disposable business enrollment fixture."""
import json
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
        export = os.environ.get("FLOWSPLICE_PTY_NATIVE_FIXTURE_EXPORT")
        if export:
            destination = Path(export)
            helper = Path(__file__).resolve().parents[2] / "scripts/private-travel-trust.py"
            command(["python3", str(helper), "check-path", "--path", str(destination)])
            destination.mkdir(mode=0o700, parents=True, exist_ok=False)
            for source, name in [(root, "deployment-root.pub"), (alpha, "business.json"), ("/business/password.txt", "password.txt")]:
                shutil.copyfile(run.directory / source.removeprefix("/business/"), destination / name)
                (destination / name).chmod(0o600)
            (destination / "fixture.json").write_text(json.dumps({"project":run.project,"image":run.image,"pty_home_container":container,"scope":scope,"business_directory":str(run.directory),"issuer_port":19084}))
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
        log = run.directory / "pty-home.log"
        if log.exists():
            destination = run.evidence / "pty-home.log"
            destination.write_text(log.read_text().replace(run.password,"[REDACTED PASSWORD]"))
            destination.chmod(0o600)
