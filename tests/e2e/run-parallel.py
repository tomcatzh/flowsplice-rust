#!/usr/bin/env python3
"""Run the E2E checks and the native Travel acceptance at once, in up to three topologies.

- l3: the E2E checks selected by --modules, once (none with --native-only).
- apple: iPhone, iPad and macOS together. They share one Relay discovery address and take turns
  stopping a Relay (tests/e2e/relay-outage.py). The simulators reach relay2 at its container
  address, which needs OrbStack to bridge the topology's network (docs/TESTING.md), so this
  topology starts first and the others follow once macOS routes it.
- android: the emulator needs another discovery address (10.0.2.2), so it gets its own topology.

Each topology runs tests/e2e/run.sh in its own checkout of the current commit (with its own
tests/e2e/generated), Compose project and block of host ports (FLOWSPLICE_E2E_PORT_OFFSET). The
E2E image is built once and shared. The summary lists every platform with its result.

Example:
  tests/e2e/run-parallel.py /tmp/flowsplice-acceptance --platforms android,iphone,ipad,macos
"""
import argparse
import json
import os
import re
import shutil
import socket
import subprocess
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
# Every host port compose.yaml publishes, and the Apple mapping ports on the Mac's loopback, as
# default numbers (see FLOWSPLICE_E2E_PORT_OFFSET).
HOST_PORTS = (10800, 10801, 10802, 11083, 11084, 11085, 11086, 11087, 11088, 11089, 11090, 11091,
              12083, 13083, 18446, 18447, 19083, 19084, 28446, 28447, 29084, 39086, 39087)
PLATFORMS = ("android", "iphone", "ipad", "macos")
APPLE = ("iphone", "ipad", "macos")

parser = argparse.ArgumentParser()
parser.add_argument("output", type=Path)
parser.add_argument("--platforms", default=",".join(PLATFORMS), help="comma-separated subset of " + ",".join(PLATFORMS))
parser.add_argument("--emulator", default=os.environ.get("ANDROID_SERIAL", "emulator-5554"),
                    help="Android emulator serial; the test clears the Travel app, so never a phone")
parser.add_argument("--iphone-simulator", default=os.environ.get("FLOWSPLICE_APPLE_IPHONE_SIMULATOR", "iPhone 17 Pro"))
parser.add_argument("--ipad-simulator", default=os.environ.get("FLOWSPLICE_APPLE_IPAD_SIMULATOR", "iPad mini (A17 Pro)"))
parser.add_argument("--port-offset", type=int, default=100,
                    help="offset of the first topology's ports from the defaults; each further topology adds 100")
parser.add_argument("--modules", default="all",
                    help='E2E checks the l3 topology runs: "all", "none" or modules (docs/TESTING.md)')
parser.add_argument("--native-only", action="store_true", help='same as --modules none')
parser.add_argument("--apple-short", action="store_true",
                    help="30 s background and 60 s sustained phases on iPhone and iPad instead of 120 s and 900 s")
parser.add_argument("--route-wait", type=int, default=600,
                    help="seconds the other topologies wait for macOS to route the apple topology")
args = parser.parse_args()
if args.native_only:
    args.modules = "none"
platforms = [name for name in args.platforms.split(",") if name]
for name in platforms:
    if name not in PLATFORMS:
        raise SystemExit(f"unknown platform {name}")
if len(set(platforms)) != len(platforms):
    raise SystemExit("name each platform once")
if not args.emulator.startswith("emulator-") and "android" in platforms:
    raise SystemExit("the Android Travel E2E clears the Travel app; --emulator must name an emulator")
units = {}
if any(name in APPLE for name in platforms):
    units["apple"] = [name for name in platforms if name in APPLE]
if args.modules != "none":
    units["l3"] = []
if "android" in platforms:
    units["android"] = ["android"]
if not units:
    raise SystemExit("nothing to run: name a platform or E2E modules")
if subprocess.run(["git", "-C", str(REPO), "status", "--porcelain"], capture_output=True, text=True, check=True).stdout.strip():
    raise SystemExit("commit or stash changes first: each topology tests a clone of HEAD")
commit = subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
offsets = {unit: args.port_offset + 100 * index for index, unit in enumerate(units)}


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


def checkout(unit):
    source = args.output / unit / "source"
    source.parent.mkdir()
    subprocess.run(["git", "clone", "--quiet", "--no-hardlinks", str(REPO), str(source)], check=True)
    subprocess.run(["git", "-C", str(source), "checkout", "--quiet", commit], check=True)
    return source


def project(unit):
    return f"flowsplice-e2e-{unit}-{os.getpid()}"


def unit_environment(unit):
    flags = {"FLOWSPLICE_ANDROID_E2E": "0", "FLOWSPLICE_APPLE_E2E": "0", "FLOWSPLICE_MACOS_E2E": "0",
             "FLOWSPLICE_E2E_MODULES": args.modules if unit == "l3" else "none"}
    if unit == "android":
        flags.update(FLOWSPLICE_ANDROID_E2E="1", ANDROID_SERIAL=args.emulator)
    elif unit == "apple":
        devices = [name for name in units["apple"] if name != "macos"]
        if devices:
            flags.update(FLOWSPLICE_APPLE_E2E="1", FLOWSPLICE_APPLE_E2E_DEVICES=" ".join(devices),
                         FLOWSPLICE_APPLE_IPHONE_SIMULATOR=args.iphone_simulator,
                         FLOWSPLICE_APPLE_IPAD_SIMULATOR=args.ipad_simulator)
        if "macos" in units["apple"]:
            flags.update(FLOWSPLICE_MACOS_E2E="1")
        if args.apple_short:
            flags.update(FLOWSPLICE_APPLE_E2E_BACKGROUND_SECONDS="30", FLOWSPLICE_APPLE_E2E_SUSTAINED_SECONDS="60")
    return dict(environment, **flags, FLOWSPLICE_E2E_SKIP_BUILD="1", FLOWSPLICE_E2E_PORT_OFFSET=str(offsets[unit]),
                FLOWSPLICE_E2E_PROJECT_NAME=project(unit))


def run_unit(unit, source):
    started = time.monotonic()
    unit_log = args.output / unit / "e2e.log"
    record = results[unit] = {"port_offset": offsets[unit], "log": str(unit_log)}
    log(unit, f"E2E starting with port offset {offsets[unit]}" + (f" for {', '.join(units[unit])}" if units[unit] else ""))
    with unit_log.open("w") as stream:
        tested = subprocess.run(["bash", "tests/e2e/run.sh"], cwd=source, stdout=stream, stderr=subprocess.STDOUT,
                                env=unit_environment(unit))
    record["exit"] = tested.returncode
    record["result"] = "passed" if tested.returncode == 0 else "failed"
    record["minutes"] = round((time.monotonic() - started) / 60, 1)
    # run.sh prints one {"native": ...} line per platform it ran.
    native = {}
    for line in unit_log.read_text(errors="replace").splitlines():
        if re.match(r'^\{"native":', line):
            entry = json.loads(line)
            native[entry.pop("native")] = entry
    record["platforms"] = native
    log(unit, f"E2E exit {tested.returncode}; {record['result']}"
        + "".join(f"; {name} {entry['result']}" for name, entry in native.items()))
    # Native Rust builds dominate the checkout's size; the logs and tests/e2e/generated stay.
    shutil.rmtree(source / "target", ignore_errors=True)


def route_interface(address):
    route = subprocess.run(["route", "-n", "get", address], capture_output=True, text=True).stdout
    return next((line.split()[-1] for line in route.splitlines() if "interface:" in line), None)


def wait_for_apple_route(thread):
    # OrbStack bridges at most 8 active Docker networks to macOS, in the order they become
    # active, so the apple topology must be active before the others take the free slots.
    default = route_interface("default")
    started = time.monotonic()
    while thread.is_alive() and time.monotonic() - started < args.route_wait:
        container = subprocess.run(
            ["docker", "ps", "-q", "--filter", f"label=com.docker.compose.project={project('apple')}",
             "--filter", "label=com.docker.compose.service=relay2"], capture_output=True, text=True).stdout.strip()
        address = subprocess.run(
            ["docker", "inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", container],
            capture_output=True, text=True).stdout.strip() if container else ""
        interface = route_interface(address) if address else None
        if interface and interface != default:
            log("apple", f"macOS routes relay2 ({address} via {interface}) after {time.monotonic() - started:.0f} s")
            return
        time.sleep(2)
    log("apple", "macOS does not route relay2 yet; starting the other topologies anyway (docs/TESTING.md)")


sources = {unit: checkout(unit) for unit in units}
log("image", f"building {image}")
with (args.output / "image-build.log").open("w") as stream:
    built = subprocess.run(["bash", "tests/e2e/run.sh"], cwd=next(iter(sources.values())), stdout=stream,
                           stderr=subprocess.STDOUT, env=dict(environment, FLOWSPLICE_E2E_BUILD_ONLY="1"))
log("image", f"build exit {built.returncode}")
try:
    if built.returncode == 0:
        threads = {unit: threading.Thread(target=run_unit, args=(unit, sources[unit])) for unit in units}
        if "apple" in threads:
            threads["apple"].start()
            if any(name != "macos" for name in units["apple"]):
                wait_for_apple_route(threads["apple"])
        for unit, thread in threads.items():
            if unit != "apple":
                thread.start()
        for thread in threads.values():
            thread.join()
    else:
        results.update({unit: {"result": "failed", "reason": "image build failed; see image-build.log"} for unit in units})
finally:
    subprocess.run(["docker", "image", "rm", "--force", image], capture_output=True, check=False)
platform_results = {}
for unit, names in units.items():
    for name in names:
        platform_results[name] = results.get(unit, {}).get("platforms", {}).get(
            name, {"result": "failed", "reason": f"did not run; see the {unit} log"})
summary = {"commit": commit, "modules": args.modules, "apple_short": args.apple_short,
           "platforms": platform_results, "topologies": results}
(args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2))
raise SystemExit(0 if all(record.get("result") == "passed" for record in results.values()) else 1)
