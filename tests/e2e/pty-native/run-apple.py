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
if len(fixture.targets) != 2:
    raise SystemExit("This acceptance requires the two-Home fixture")
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
if args.platform == "macos":
    entry = manifest["platforms"]["macos"]
    expected = entry.get("bundle_id", "")
    if not isinstance(expected, str) or re.fullmatch(r"io\.zxf\.flowsplice\.pty\.macos\.e2e\.[0-9a-f]{32}", expected) is None or entry.get("test_only_bundle_id") is not True or entry.get("code_signing") != "ad-hoc":
        raise SystemExit("Refusing a macOS test manifest without isolated application identity")
    app = Path(entry["app"]).resolve(strict=True)
    if plistlib.loads((app / "Contents/Info.plist").read_bytes()).get("CFBundleIdentifier") != expected:
        raise SystemExit("Refusing macOS UI tests against the production application")
    def mac_targets(value):
        if isinstance(value, dict):
            if "TestBundlePath" in value: yield value
            for child in value.values(): yield from mac_targets(child)
        elif isinstance(value, list):
            for child in value: yield from mac_targets(child)
    mac = list(mac_targets(run))
    if len(mac) != 1:
        raise SystemExit("Expected exactly one isolated macOS UI test target")
    target = mac[0]
    # Current Xcode emits UITargetAppPath, not TestTargetBundleIdentifier.
    # Verify that resolved target bundle directly; reject an explicit ID if present.
    target_path = Path(target.get("UITargetAppPath", "").replace("__TESTROOT__", str(original.parent))).resolve(strict=True)
    if target_path != app or target.get("TestTargetBundleIdentifier", expected) != expected:
        raise SystemExit("macOS xctestrun points outside the isolated test application")
    if target.get("TestHostBundleIdentifier") != expected + ".uitests.xctrunner":
        raise SystemExit("macOS xctestrun runner identity is not isolated")
    runner = Path(target["TestHostPath"].replace("__TESTROOT__", str(original.parent))).resolve(strict=True)
    if plistlib.loads((runner / "Contents/Info.plist").read_bytes()).get("CFBundleIdentifier") != expected + ".uitests.xctrunner":
        raise SystemExit("macOS runner bundle differs from isolated identity")
    for bundle in [app, runner]:
        subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle)], check=True)
        signature = subprocess.run(["codesign", "-d", "--verbose=4", str(bundle)], capture_output=True, text=True, check=True)
        lines = {line.strip() for line in (signature.stdout + signature.stderr).splitlines()}
        if "Signature=adhoc" not in lines or any(line.startswith("Authority=") for line in lines):
            raise SystemExit("macOS tests require valid ad hoc signatures without certificate authority")
    if target.get("BundleIdentifiersForCrashReportEmphasis") != [expected, expected + ".uitests"]:
        raise SystemExit("macOS xctestrun application identities are not isolated")
environment = {
    "DEVELOPER_DIR": os.environ["DEVELOPER_DIR"],
    "FLOWSPLICE_PTY_E2E_ISOLATED": "1",
    "FLOWSPLICE_PTY_E2E_RELAY": "127.0.0.1:18446",
    "FLOWSPLICE_PTY_E2E_PASSWORD_FILE": str(fixture.directory / "password.txt"),
}
if fixture.service_class:
    environment.update({
        "FLOWSPLICE_PTY_E2E_SERVICE_CLASS":"1",
        "FLOWSPLICE_PTY_E2E_FIRST_HOME":fixture.native_targets[0]["name"],
        "FLOWSPLICE_PTY_E2E_SECOND_HOME":fixture.native_targets[1]["name"],
    })
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
approvals = {}
deletion = None
with log.open("wb") as output:
    process = subprocess.Popen(["xcodebuild", "test-without-building", "-xctestrun", str(configured),
        "-destination", destination, "-parallel-testing-enabled", "NO", "-resultBundlePath", str(result)],
        stdout=output, stderr=subprocess.STDOUT)
    # Full history gestures/OCR plus continuity can exceed fifteen minutes on a
    # loaded simulator. Keep one supervisor alive for the entire bounded run.
    deadline = time.monotonic() + 1800
    try:
        while process.poll() is None:
            if time.monotonic() > deadline:
                raise RuntimeError("Apple PTY UI test exceeded its deadline")
            rendered_log = log.read_text(errors="replace")
            if deletion is None and "PTY_E2E_DELETE_SECOND_SESSION" in rendered_log:
                deletion = fixture.delete_second_session()
            labels = re.findall(r"PTY_E2E_IDENTITY ([^\r\n]+)", rendered_log)
            client_label = labels[-1] if labels else None
            for notice in re.findall(r"PTY_E2E_VERIFICATION ([^\r\n]+)", rendered_log):
                if notice not in approvals:
                    approved = fixture.approve_notice(notice, client_label)
                    if approved is not None: approvals[notice] = approved
            time.sleep(0.5)
        if process.returncode != 0 or len(approvals) != fixture.expected_approvals or deletion is None:
            raise RuntimeError("Apple private PTY UI acceptance failed; inspect the xcresult")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
acceptance.write_text(json.dumps({"platform":args.platform,"passed":True,
    "result":str(result),"completed_at_unix_ns":time.time_ns(),"approvals":list(approvals.values()),"external_deletion":deletion}, indent=2))
print(json.dumps({"checkpoint":"private-apple-pty-ui-passed","platform":args.platform,"result":str(result)}))
