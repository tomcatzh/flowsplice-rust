#!/usr/bin/env python3
"""Build pinned reference codecs outside the production workspace (macOS/Linux)."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import urllib.request

SOURCES = [
    ("snappy", "1.2.2", "https://codeload.github.com/google/snappy/tar.gz/refs/tags/1.2.2", "90f74bc1fbf78a6c56b3c4a082a05103b3a56bb17bca1a27e052ea11723292dc"),
    ("zstd", "1.5.7", "https://codeload.github.com/facebook/zstd/tar.gz/refs/tags/v1.5.7", "37d7284556b20954e56e1ca85b80226768902e2edabd3b649e9e72c0c9012ee3"),
    ("lz4", "1.10.0", "https://codeload.github.com/lz4/lz4/tar.gz/refs/tags/v1.10.0", "537512904744b35e232912055ccf8ec66d768639ff3abe5788d90d792ec5f48b"),
]

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    root = args.directory.resolve()
    lib = root / "lib"
    lib.mkdir(parents=True, exist_ok=True)
    commands = []
    def run(command):
        commands.append([str(x) for x in command])
        subprocess.run(command, check=True)
    dirs = {}
    for name, version, url, sha in SOURCES:
        archive = root / f"{name}-{version}.tar.gz"
        if not archive.exists():
            urllib.request.urlretrieve(url, archive)
        actual = hashlib.sha256(archive.read_bytes()).hexdigest()
        if actual != sha:
            raise RuntimeError(f"SHA-256 mismatch for {archive}: {actual}")
        source = root / f"{name}-{version}"
        if not source.exists():
            with tarfile.open(archive) as tf:
                tf.extractall(root, filter="data")
        dirs[name] = source
    common = ["-DCMAKE_BUILD_TYPE=Release", "-DCMAKE_C_FLAGS_RELEASE=-O3 -DNDEBUG",
              "-DCMAKE_CXX_FLAGS_RELEASE=-O3 -DNDEBUG", "-DCMAKE_POLICY_VERSION_MINIMUM=3.5"]
    snappy_build = root / "snappy-build"
    run(["cmake", "-S", dirs["snappy"], "-B", snappy_build, *common,
         "-DSNAPPY_BUILD_TESTS=OFF", "-DSNAPPY_BUILD_BENCHMARKS=OFF", "-DBUILD_SHARED_LIBS=OFF"])
    run(["cmake", "--build", snappy_build, "--parallel", "4"])
    shutil.copy2(snappy_build / "libsnappy.a", lib)
    zstd_build = root / "zstd-build"
    run(["cmake", "-S", dirs["zstd"] / "build/cmake", "-B", zstd_build, *common,
         "-DZSTD_BUILD_PROGRAMS=OFF", "-DZSTD_BUILD_TESTS=OFF", "-DZSTD_BUILD_SHARED=OFF",
         "-DZSTD_MULTITHREAD_SUPPORT=OFF"])
    run(["cmake", "--build", zstd_build, "--target", "libzstd_static", "--parallel", "4"])
    shutil.copy2(zstd_build / "lib/libzstd.a", lib)
    cc = os.environ.get("CC", "cc")
    cxx = os.environ.get("CXX", "c++")
    for file in ["lz4", "lz4hc"]:
        run([cc, "-O3", "-DNDEBUG", "-c", dirs["lz4"] / f"lib/{file}.c", "-o", root / f"{file}.o"])
    run(["ar", "rcs", lib / "liblz4.a", root / "lz4.o", root / "lz4hc.o"])
    run([cxx, "-std=c++17", "-O3", "-DNDEBUG", "-I" + str(dirs["snappy"]),
         "-I" + str(snappy_build), "-I" + str(dirs["zstd"] / "lib"),
         "-I" + str(dirs["lz4"] / "lib"), "-c", Path(__file__).with_name("native.cc"),
         "-o", root / "bench_native.o"])
    run(["ar", "rcs", lib / "libbench_native.a", root / "bench_native.o"])
    manifest = {"sources": [{"name": n, "version": v, "url": u, "sha256": s} for n, v, u, s in SOURCES],
                "commands": commands, "compiler": subprocess.check_output([cxx, "--version"], text=True),
                "archives": {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in lib.glob("*.a")}}
    (root / "native-build.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Ready. BENCH_NATIVE_DIR={root}")

if __name__ == "__main__":
    main()
