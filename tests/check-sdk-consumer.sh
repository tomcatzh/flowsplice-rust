#!/usr/bin/env bash
# SDK_GIT_REV=<commit-or-tag> checks the public Git repository instead of local source.
# CARGO_TARGET_DIR may point to a reusable build cache. SDK_SOURCE selects a local checkout.
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
stage="$(mktemp -d "${TMPDIR:-/tmp}/flowsplice-sdk-consumer.XXXXXX")"
trap 'rm -rf -- "$stage"' EXIT
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${stage}/target}"

python3 - "$repository_root" "$stage" <<'PY'
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

repository, stage = map(Path, sys.argv[1:])
source = Path(os.environ.get("SDK_SOURCE", repository)).resolve()
revision = os.environ.get("SDK_GIT_REV")
if revision:
    dependency = 'git = "https://github.com/tomcatzh/flowsplice-rust", rev = ' + json.dumps(revision)
else:
    snapshot = stage / "source"
    # Include current edits and new source files, but exclude ignored build products.
    paths = subprocess.check_output([
        "git", "-C", str(source), "ls-files", "--cached", "--others", "--exclude-standard", "-z"
    ])
    for raw in paths.split(b"\0"):
        if not raw:
            continue
        relative = Path(os.fsdecode(raw))
        original = source / relative
        if original.is_file():
            destination = snapshot / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(original, destination)
    assert not (snapshot / "travelagent/web/dist").exists(), "unexpected tracked frontend build"

for mode in ("default", "no-default"):
    consumer = stage / mode
    (consumer / "src").mkdir(parents=True)
    shutil.copy2(repository / "tests/fixtures/sdk-consumer/main.rs", consumer / "src/main.rs")
    lines = ['[package]', 'name = "sdk-consumer-' + mode + '"', 'version = "0.0.0"',
             'edition = "2024"', '[workspace]', '[dependencies]', 'anyhow = "1"']
    for name in ("flowsplice-home-core", "flowsplice-travel-core"):
        location = dependency if revision else 'path = ' + json.dumps(str(snapshot / "crates" / name))
        features = ', default-features = false' if mode == "no-default" else ''
        lines.append(name + ' = { ' + location + features + ' }')
    (consumer / "Cargo.toml").write_text('\n'.join(lines) + '\n')
PY

for mode in default no-default; do
    cargo run --manifest-path "${stage}/${mode}/Cargo.toml"
done
