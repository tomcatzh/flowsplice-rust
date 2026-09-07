#!/usr/bin/env python3
"""Execute built Apple UI tests with external fixture approval and isolated app state."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess
import time
from fixture import Fixture, command

parser = argparse.ArgumentParser()
parser.add_argument("fixture")
parser.add_argument("build")
parser.add_argument("--platform", choices=["iphone", "ipad", "macos"], required=True)
parser.add_argument("--device")
args = parser.parse_args()
os.environ.setdefault("DEVELOPER_DIR", "/Applications/Xcode.app/Contents/Developer")
fixture = Fixture(args.fixture)
build = Path(args.build).resolve(strict=True)
acceptance = build / (args.platform + "-acceptance.json")
if acceptance.exists():
    acceptance.rename(build / (args.platform + "-acceptance-previous-" + str(time.time_ns()) + ".json"))
manifest = json.loads((build / "manifest.json").read_text())
platform = "macos" if args.platform == "macos" else "ios"
original = Path(manifest["platforms"][platform]["xctestrun"])
if args.platform == "macos":
    destination = "platform=macOS,arch=arm64"
else:
    devices = json.loads(command(["xcrun", "simctl", "list", "devices", "--json"]).stdout)["devices"]
    device = next((d for group in devices.values() for d in group if d["udid"] == args.device), None)
    expected_name = "FlowSplice-04-iPhone" if args.platform == "iphone" else "FlowSplice-04-iPad"
    if device is None or device["name"] != expected_name:
        raise SystemExit("Only the dedicated FlowSplice 0.4 simulator is allowed")
    if device["state"] != "Booted":
        command(["xcrun", "simctl", "boot", args.device])
    command(["xcrun", "simctl", "bootstatus", args.device, "-b"])
    destination = "platform=iOS Simulator,id=" + args.device

with original.open("rb") as stream:
    run = plistlib.load(stream)
environment = {
    "DEVELOPER_DIR": os.environ["DEVELOPER_DIR"],
    "FLOWSPLICE_PTY_E2E_ISOLATED": "1",
    "FLOWSPLICE_PTY_E2E_RELAY": "127.0.0.1:18446",
    "FLOWSPLICE_PTY_E2E_PASSWORD_FILE": str(fixture.directory / "password.txt"),
}
targets = []
def configure(value):
    if isinstance(value, dict):
        if "TestBundlePath" in value:
            for key in ["EnvironmentVariables", "TestingEnvironmentVariables"]:
                value.setdefault(key, {}).update(environment)
            value["ParallelizationEnabled"] = False
            targets.append(value)
        else:
            for child in value.values():
                configure(child)
    elif isinstance(value, list):
        for child in value:
            configure(child)
configure(run)
if len(targets) != 1:
    raise RuntimeError("Expected one private PTY UI test target")
configured = original.with_name("private-pty-" + args.platform + ".xctestrun")
with configured.open("wb") as stream:
    plistlib.dump(run, stream)
fixture.clear_test_sessions()
log = build / (args.platform + "-private-ui-e2e.log")
if log.exists():
    log.rename(build / (args.platform + "-private-ui-e2e-previous-" + str(time.time_ns()) + ".log"))
result = build / (args.platform + "-private-ui-" + str(time.time_ns()) + ".xcresult")
approval = None
with log.open("wb") as output:
    process = subprocess.Popen(["xcodebuild", "test-without-building", "-xctestrun", str(configured),
        "-destination", destination, "-parallel-testing-enabled", "NO", "-resultBundlePath", str(result)],
        stdout=output, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 360
    try:
        while process.poll() is None:
            if time.monotonic() > deadline:
                raise RuntimeError("Apple PTY UI test exceeded its deadline")
            if approval is None:
                match = re.search(r"PTY_E2E_VERIFICATION ([^\r\n]+)", log.read_text(errors="replace"))
                if match:
                    approval = fixture.approve_notice(match.group(1))
            time.sleep(0.5)
        if process.returncode != 0 or approval is None:
            raise RuntimeError("Apple private PTY UI acceptance failed; inspect the xcresult")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
acceptance.write_text(json.dumps({"platform":args.platform,"passed":True,
    "result":str(result),"completed_at_unix_ns":time.time_ns(),**approval}, indent=2))
print(json.dumps({"checkpoint":"private-apple-pty-ui-passed","platform":args.platform,"result":str(result)}))
