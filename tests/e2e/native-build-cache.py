#!/usr/bin/env python3
"""Reuse a native E2E test build when nothing it is built from has changed.

The iOS, macOS and Android test builds depend on the program sources, their own platform
directory, the toolchains and the test deployment root they bundle (kept stable on this Mac, see
tests/e2e/generate-certs.sh). The key hashes exactly those, so a change to tests, docs, the backend
services or another platform reuses the cached products instead of building again. One lock per
platform makes the iPhone and iPad runs of a topology build at most once.

FLOWSPLICE_E2E_NATIVE_CACHE=0 always builds. The cache lives in FLOWSPLICE_E2E_CACHE_DIR
(default ~/Library/Caches/FlowSpliceE2E) and keeps the newest three builds per platform.

Example:
  tests/e2e/native-build-cache.py ios "$derived/Build/Products" --input "$root" --output "$log" -- \
    xcodebuild ... build-for-testing
"""
import argparse
import fcntl
import hashlib
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
# The same files tests/e2e/stage-source.py stages for a native build.
EXCLUDED_PARTS = {".git", ".idea", ".kotlin", ".llmwiki", ".agents", ".codex", "build", "target", "dist", "node_modules"}
EXCLUDED_NAMES = {"AGENTS.md", "CLAUDE.md", "local.properties"}
# None of these is built into a Travel app.
NOT_IN_APPS = ("tests/", "docs/", ".github/", "server/", "relay/", "homeagent/", "travelagent/", "foobar/",
               "crates/flowsplice-home-core/")
OTHER_PLATFORMS = {
    "ios": ("travel-android/", "travel-macos/"),
    "macos": ("travel-android/", "travel-apple/"),
    "android": ("travel-apple/", "travel-macos/"),
}
TOOLCHAINS = {
    "ios": (["xcodebuild", "-version"], ["xcrun", "--sdk", "iphonesimulator", "--show-sdk-version"], ["rustc", "-Vv"]),
    "macos": (["xcodebuild", "-version"], ["xcrun", "--sdk", "macosx", "--show-sdk-version"], ["rustc", "-Vv"]),
    "android": (["rustc", "-Vv"], [str(Path(os.environ.get("JAVA_HOME", "/usr")) / "bin/java"), "-version"]),
}
KEEP = 3

parser = argparse.ArgumentParser()
parser.add_argument("platform", choices=sorted(OTHER_PLATFORMS))
parser.add_argument("products", type=Path, help="directory the build command fills")
parser.add_argument("--input", type=Path, action="append", default=[], help="another file the build bundles")
parser.add_argument("--extra", action="append", default=[], help="another value the build depends on")
parser.add_argument("--output", type=Path, help="file for the build command's output")
parser.add_argument("command", nargs="+")
args = parser.parse_args()


def build():
    if args.output:
        with args.output.open("a") as stream:
            return subprocess.run(args.command, stdout=stream, stderr=subprocess.STDOUT).returncode
    return subprocess.run(args.command).returncode


def key():
    listed = subprocess.run(["git", "-C", str(REPO), "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
                            capture_output=True, check=True).stdout.decode()
    skipped = NOT_IN_APPS + OTHER_PLATFORMS[args.platform]
    paths = sorted(
        path for path in set(listed.split("\0"))
        if path and not path.startswith(skipped) and not EXCLUDED_PARTS.intersection(Path(path).parts)
        and Path(path).name not in EXCLUDED_NAMES and (REPO / path).is_file()
    )
    blobs = subprocess.run(["git", "-C", str(REPO), "hash-object", "--stdin-paths"], input="\n".join(paths),
                           capture_output=True, text=True, check=True).stdout.split()
    digest = hashlib.sha256(f"flowsplice-native-build-1 {args.platform}\n".encode())
    for path, blob in zip(paths, blobs, strict=True):
        digest.update(f"{path} {blob} {os.access(REPO / path, os.X_OK)}\n".encode())
    for command in TOOLCHAINS[args.platform]:
        # Error text varies between calls; only a working tool's version belongs in the key.
        found = subprocess.run(command, cwd=REPO, capture_output=True, text=True)
        version = (found.stdout or found.stderr).strip() if found.returncode == 0 else "unavailable"
        digest.update(f"{command} {version}\n".encode())
    for path in args.input:
        digest.update(f"input {hashlib.sha256(path.read_bytes()).hexdigest()}\n".encode())
    for value in args.extra:
        digest.update(f"extra {value}\n".encode())
    return digest.hexdigest()


def copy(source, destination):
    # clonefile(2) on APFS makes this instant; plain copies elsewhere.
    shutil.rmtree(destination, ignore_errors=True)
    destination.parent.mkdir(parents=True, exist_ok=True)
    if subprocess.run(["cp", "-cR", str(source), str(destination)], capture_output=True).returncode != 0:
        shutil.rmtree(destination, ignore_errors=True)
        subprocess.run(["cp", "-R", str(source), str(destination)], check=True)


if os.environ.get("FLOWSPLICE_E2E_NATIVE_CACHE", "1") == "0":
    raise SystemExit(build())
cache = Path(os.environ.get("FLOWSPLICE_E2E_CACHE_DIR") or Path.home() / "Library/Caches/FlowSpliceE2E") / "native"
platform_cache = cache / args.platform
platform_cache.mkdir(parents=True, exist_ok=True)
build_key = key()
entry = platform_cache / build_key
with (cache / f"{args.platform}.lock").open("w") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    if (entry / "complete").is_file():
        copy(entry / "products", args.products)
        os.utime(entry)
        print(f"{args.platform} test build reused from the cache ({build_key[:12]})", file=sys.stderr)
        raise SystemExit(0)
    started = time.monotonic()
    status = build()
    if status != 0:
        raise SystemExit(status)
    if not args.products.is_dir():
        raise SystemExit(f"the {args.platform} build did not produce {args.products}")
    staging = platform_cache / f".{build_key}.{os.getpid()}"
    shutil.rmtree(staging, ignore_errors=True)
    copy(args.products, staging / "products")
    (staging / "complete").write_text(f"built in {time.monotonic() - started:.0f} s\n")
    shutil.rmtree(entry, ignore_errors=True)
    staging.rename(entry)
    older = sorted((path for path in platform_cache.iterdir() if (path / "complete").is_file()),
                   key=lambda path: path.stat().st_mtime, reverse=True)[KEEP:]
    for path in older:
        shutil.rmtree(path, ignore_errors=True)
    print(f"{args.platform} test built in {time.monotonic() - started:.0f} s and cached ({build_key[:12]})", file=sys.stderr)
