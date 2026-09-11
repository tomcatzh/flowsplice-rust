#!/usr/bin/env python3
"""Build, validate, record the host, then execute the Rust timing harness."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import time

def capture(args):
    p = subprocess.run(args, capture_output=True, text=True)
    return {"command": args, "exit_code": p.returncode,
            "stdout": p.stdout.strip(), "stderr": p.stderr.strip()}

def host():
    result = {"machine": platform.machine(), "system": platform.system(),
              "release": platform.release(), "logical_cpu_count": os.cpu_count(),
              "load_averages": os.getloadavg(), "rustc": capture(["rustc", "-Vv"]),
              "clang": capture(["clang", "--version"])}
    if platform.system() == "Darwin":
        result["macos"] = capture(["sw_vers"])
        # Do not record serial numbers, UUIDs or account information.
        result["cpu"] = capture(["sysctl", "-n", "machdep.cpu.brand_string"])
        result["memory_bytes"] = capture(["sysctl", "-n", "hw.memsize"])
        result["power"] = capture(["pmset", "-g", "batt"])
    return result

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("native", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--sample-ms", type=int, default=120)
    args = parser.parse_args()
    here = Path(__file__).resolve().parent
    repo = here.parent.parent
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    native = args.native.resolve()
    env = dict(os.environ, BENCH_NATIVE_DIR=str(native))
    manifest = here / "Cargo.toml"
    for action in ["test", "build"]:
        subprocess.run(["cargo", action, "--release", "--locked", "--manifest-path", str(manifest)],
                       check=True, env=env)
    binary = here / "target/release/flowsplice-compression-bench"
    before = host()
    sources = ["Cargo.toml", "Cargo.lock", "build.rs", "native.cc", "src/main.rs", "src/corpus.rs", "src/frozen.rs", "prepare_native.py"]
    metadata = {"before": before, "after": None,
                "git_revision": capture(["git", "-C", str(repo), "rev-parse", "HEAD"])["stdout"],
                "source_sha256": {p: hashlib.sha256((here / p).read_bytes()).hexdigest() for p in sources},
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "started_unix_seconds": time.time(), "completed": False}
    shutil.copy2(native / "native-build.json", output / "native-build.json")
    (output / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    with (output / "run.log").open("w") as log:
        command = [str(binary), str(repo), str(output), str(args.rounds), str(args.sample_ms)]
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   text=True, env=env)
        for line in process.stdout:
            print(line, end="", flush=True)
            log.write(line)
            log.flush()
        status = process.wait()
    metadata.update(after=host(), completed=status == 0, exit_code=status,
                    finished_unix_seconds=time.time())
    (output / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    if status:
        raise SystemExit(status)

if __name__ == "__main__":
    main()
