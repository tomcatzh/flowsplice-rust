#!/usr/bin/env python3
"""Run private PTY UI acceptance on the dedicated emulator, with matched approval."""
import argparse
import base64
import json
import os
from pathlib import Path
import subprocess
import time
from fixture import Fixture, command

parser = argparse.ArgumentParser()
parser.add_argument("fixture")
parser.add_argument("build")
parser.add_argument("--serial", required=True)
args = parser.parse_args()
if not args.serial.startswith("emulator-"):
    raise SystemExit("This runner accepts a dedicated emulator only")
fixture = Fixture(args.fixture)
if len(fixture.targets) != 2:
    raise SystemExit("This acceptance requires the two-Home fixture")
build = Path(args.build).resolve(strict=True)
acceptance = build / "acceptance.json"
if acceptance.exists():
    acceptance.rename(build / ("acceptance-previous-" + str(time.time_ns()) + ".json"))
adb = str(Path(os.environ["ANDROID_SDK_ROOT"]) / "platform-tools/adb")
base = [adb, "-s", args.serial]
avd = command(base + ["emu", "avd", "name"]).stdout.decode().splitlines()[0]
if avd != "FlowSplice_04_E2E":
    raise SystemExit("Refusing to change an emulator outside the dedicated PTY acceptance fixture")
package = "io.zxf.flowsplice.pty"
products = build / "source/pty-android/app/build/outputs/apk"
fixture.clear_test_sessions()
# This distinct package on the dedicated emulator contains disposable identities only.
subprocess.run(base + ["uninstall", package], capture_output=True, timeout=30, check=False)
subprocess.run(base + ["uninstall", package + ".test"], capture_output=True, timeout=30, check=False)
command(base + ["install", str(products / "debug/app-debug.apk")])
command(base + ["install", "-r", str(products / "androidTest/debug/app-debug-androidTest.apk")])
command(base + ["shell", "run-as", package, "mkdir", "-p", "files"])
staged_password = "/data/local/tmp/flowsplice-pty-fixture-" + str(time.time_ns())
try:
    command(base + ["push", str(fixture.directory / "password.txt"), staged_password])
    command(base + ["shell", "run-as", package, "cp", staged_password, "files/fixture-password"])
    command(base + ["shell", "run-as", package, "chmod", "600", "files/fixture-password"])
finally:
    command(base + ["shell", "rm", "-f", staged_password])
copied = command(base + ["exec-out", "run-as", package, "cat", "files/fixture-password"]).stdout
if copied.rstrip(b"\r\n") != fixture.password.encode():
    raise RuntimeError("Android private fixture password did not copy exactly")
log = build / "private-ui-e2e.log"
if log.exists():
    log.rename(build / ("private-ui-e2e-previous-" + str(time.time_ns()) + ".log"))
approvals = {}
class_arguments = []
if fixture.service_class:
    class_arguments = ["-e", "ptyServiceClass", "true"]
    for prefix, target in zip(["ptyFirst", "ptySecond"], fixture.native_targets):
        for suffix, key in [("HomeId", "id"), ("HomeName", "name")]:
            class_arguments += ["-e", prefix + suffix + "Base64", base64.b64encode(target[key].encode()).decode()]
with log.open("wb") as output:
    process = subprocess.Popen(base + ["shell", "am", "instrument", "-w", "-r",
        "-e", "class", "io.zxf.flowsplice.pty.PrivateTerminalE2ETest",
        "-e", "isolated", "true", "-e", "ptyRelay", "10.0.2.2:18446",
        "-e", "ptyPasswordFile", "/data/user/0/" + package + "/files/fixture-password",
        *class_arguments, package + ".test/androidx.test.runner.AndroidJUnitRunner"], stdout=output, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 720
    try:
        while process.poll() is None:
            if time.monotonic() > deadline:
                raise RuntimeError("Android PTY UI test exceeded its deadline")
            if len(approvals) < fixture.expected_approvals:
                result = subprocess.run(base + ["exec-out", "run-as", package, "cat", "files/e2e-verification.json"],
                                        capture_output=True, timeout=10, check=False)
                if result.returncode == 0 and result.stdout.strip():
                    try:
                        notice = json.loads(result.stdout)
                    except json.JSONDecodeError:
                        # exec-out may return remote cat errors on stdout with host exit 0.
                        notice = ""
                    if isinstance(notice, (str, dict)):
                        notice_key = json.dumps(notice, sort_keys=True)
                        if notice_key not in approvals:
                            approved = fixture.approve_notice(notice)
                            if approved is not None: approvals[notice_key] = approved
            time.sleep(0.5)
        if process.returncode != 0 or len(approvals) != fixture.expected_approvals or "OK (1 test)" not in log.read_text():
            raise RuntimeError("Android private PTY UI acceptance failed; see private-ui-e2e.log")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
        command(base + ["shell", "run-as", package, "rm", "-f", "files/fixture-password"])
acceptance.write_text(json.dumps({"platform":"android", "passed":True,
    "log":str(log),"completed_at_unix_ns":time.time_ns(), "approvals":list(approvals.values())}, indent=2))
print(json.dumps({"checkpoint":"private-android-pty-ui-passed", "evidence":str(build / "acceptance.json")}))
