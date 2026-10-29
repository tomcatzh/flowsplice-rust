#!/usr/bin/env python3
"""Run the native Travel acceptance on several platforms at once, one E2E topology per platform.

Platforms cannot share one topology: the Apple and macOS tests stop and restart a Relay while their
app is connected, and Android needs another Relay discovery address than the Apple platforms.
Each platform therefore runs tests/e2e/run.sh in its own checkout of the current commit (with its
own tests/e2e/generated), Compose project and block of host ports (FLOWSPLICE_E2E_PORT_OFFSET), with
only its own native phase enabled. --modules selects the E2E checks each topology runs first
(--native-only runs none). The E2E image is built once and shared. The summary lists every
platform with its log.

Example:
  tests/e2e/run-parallel.py /tmp/flowsplice-acceptance --platforms android,iphone,ipad,macos
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
# Every host port compose.yaml publishes, as default numbers (see FLOWSPLICE_E2E_PORT_OFFSET).
HOST_PORTS = (11083, 11084, 11085, 11086, 11087, 11088, 11089, 11090, 11091, 12083, 13083,
              18446, 18447, 19083, 19084, 28446, 28447, 29084, 39086, 39087)
PLATFORMS = ("android", "iphone", "ipad", "macos")

parser = argparse.ArgumentParser()
parser.add_argument("output", type=Path)
parser.add_argument("--platforms", default=",".join(PLATFORMS), help="comma-separated subset of " + ",".join(PLATFORMS))
parser.add_argument("--emulator", default=os.environ.get("ANDROID_SERIAL", "emulator-5554"),
                    help="Android emulator serial; the test clears the Travel app, so never a phone")
parser.add_argument("--iphone-simulator", default=os.environ.get("FLOWSPLICE_APPLE_IPHONE_SIMULATOR", "iPhone 17 Pro"))
parser.add_argument("--ipad-simulator", default=os.environ.get("FLOWSPLICE_APPLE_IPAD_SIMULATOR", "iPad mini (A17 Pro)"))
parser.add_argument("--port-offset", type=int, default=100,
                    help="offset of the first platform's ports from the defaults; each further platform adds 100")
parser.add_argument("--modules", default="all",
                    help='E2E checks each topology runs before its native phase: "all", "none" or modules (docs/TESTING.md)')
parser.add_argument("--native-only", action="store_true", help='same as --modules none')
parser.add_argument("--apple-short", action="store_true",
                    help="30 s background and 60 s sustained phases on iPhone and iPad instead of 120 s and 900 s")
args = parser.parse_args()
if args.native_only:
    args.modules = "none"
platforms = [name for name in args.platforms.split(",") if name]
for name in platforms:
    if name not in PLATFORMS:
        raise SystemExit(f"unknown platform {name}")
if not platforms or len(set(platforms)) != len(platforms):
    raise SystemExit("name each platform once")
if not args.emulator.startswith("emulator-") and "android" in platforms:
    raise SystemExit("the Android Travel E2E clears the Travel app; --emulator must name an emulator")
if subprocess.run(["git", "-C", str(REPO), "status", "--porcelain"], capture_output=True, text=True, check=True).stdout.strip():
    raise SystemExit("commit or stash changes first: each platform tests a clone of HEAD")
commit = subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
offsets = {name: args.port_offset + 100 * index for index, name in enumerate(platforms)}


def port_in_use(port):
    with socket.socket() as probe:
        return probe.connect_ex(("127.0.0.1", port)) == 0


busy = sorted(port + offset for offset in offsets.values() for port in HOST_PORTS if port_in_use(port + offset))
if busy:
    raise SystemExit(f"host ports in use: {busy}; choose another --port-offset")
args.output.mkdir(parents=True, exist_ok=False)
image = f"flowsplice-e2e-parallel-{os.getpid()}:local"
environment = dict(os.environ, DEVELOPER_DIR=os.environ.get("DEVELOPER_DIR", "/Applications/Xcode.app/Contents/Developer"),
                   ANDROID_SDK_ROOT=os.environ.get("ANDROID_SDK_ROOT", str(Path.home() / "Library/Android/sdk")),
                   FLOWSPLICE_E2E_IMAGE=image)
log_lock = threading.Lock()
results = {}


def log(name, message):
    line = f"{time.strftime('%H:%M:%S')} {name}: {message}"
    with log_lock:
        print(line, flush=True)
        with (args.output / "progress.log").open("a") as stream:
            stream.write(line + "\n")


def checkout(name):
    source = args.output / name / "source"
    source.parent.mkdir()
    subprocess.run(["git", "clone", "--quiet", "--no-hardlinks", str(REPO), str(source)], check=True)
    subprocess.run(["git", "-C", str(source), "checkout", "--quiet", commit], check=True)
    return source


def platform_environment(name):
    flags = {"FLOWSPLICE_ANDROID_E2E": "0", "FLOWSPLICE_APPLE_E2E": "0", "FLOWSPLICE_MACOS_E2E": "0"}
    if name == "android":
        flags.update(FLOWSPLICE_ANDROID_E2E="1", ANDROID_SERIAL=args.emulator)
    elif name == "macos":
        flags.update(FLOWSPLICE_MACOS_E2E="1")
    else:
        flags.update(FLOWSPLICE_APPLE_E2E="1", FLOWSPLICE_APPLE_E2E_DEVICES=name,
                     FLOWSPLICE_APPLE_IPHONE_SIMULATOR=args.iphone_simulator,
                     FLOWSPLICE_APPLE_IPAD_SIMULATOR=args.ipad_simulator)
    if args.apple_short:
        flags.update(FLOWSPLICE_APPLE_E2E_BACKGROUND_SECONDS="30", FLOWSPLICE_APPLE_E2E_SUSTAINED_SECONDS="60")
    return dict(environment, **flags, FLOWSPLICE_E2E_SKIP_BUILD="1", FLOWSPLICE_E2E_PORT_OFFSET=str(offsets[name]),
                FLOWSPLICE_E2E_PROJECT_NAME=f"flowsplice-e2e-{name}-{os.getpid()}", FLOWSPLICE_E2E_MODULES=args.modules)


def run_platform(name, source):
    started = time.monotonic()
    record = results[name] = {"port_offset": offsets[name], "log": str(args.output / name / "e2e.log")}
    log(name, f"E2E starting with port offset {offsets[name]}")
    with (args.output / name / "e2e.log").open("w") as stream:
        tested = subprocess.run(["bash", "tests/e2e/run.sh"], cwd=source, stdout=stream, stderr=subprocess.STDOUT,
                                env=platform_environment(name))
    record["exit"] = tested.returncode
    record["result"] = "passed" if tested.returncode == 0 else "failed"
    record["minutes"] = round((time.monotonic() - started) / 60, 1)
    log(name, f"E2E exit {tested.returncode}; {record['result']}")
    # Native Rust builds dominate the checkout's size; the logs and tests/e2e/generated stay.
    shutil.rmtree(source / "target", ignore_errors=True)


sources = {name: checkout(name) for name in platforms}
log("image", f"building {image}")
with (args.output / "image-build.log").open("w") as stream:
    built = subprocess.run(["bash", "tests/e2e/run.sh"], cwd=sources[platforms[0]], stdout=stream, stderr=subprocess.STDOUT,
                           env=dict(environment, FLOWSPLICE_E2E_BUILD_ONLY="1"))
log("image", f"build exit {built.returncode}")
try:
    if built.returncode == 0:
        threads = [threading.Thread(target=run_platform, args=(name, sources[name])) for name in platforms]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
    else:
        results.update({name: {"result": "failed", "reason": "image build failed; see image-build.log"} for name in platforms})
finally:
    subprocess.run(["docker", "image", "rm", "--force", image], capture_output=True, check=False)
summary = {"commit": commit, "modules": args.modules, "apple_short": args.apple_short, "platforms": results}
(args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2))
raise SystemExit(0 if all(record.get("result") == "passed" for record in results.values()) else 1)
