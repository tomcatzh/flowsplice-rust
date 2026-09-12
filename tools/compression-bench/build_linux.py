#!/usr/bin/env python3
"""Pinned, cached-image-only static Linux benchmark builder; no remote execution."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile

IMAGE = 'rust:1.97-alpine@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900'


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--stage', choices=['native', 'final'], required=True)
    p.add_argument('--arch', choices=['amd64', 'arm64'], required=True)
    p.add_argument('--output', type=Path, default=Path('/private/tmp/flowsplice-compression-linux'))
    p.add_argument('--sources', type=Path, default=Path('/private/tmp/flowsplice-compression-bench/native'))
    p.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    a = p.parse_args()
    if not a.inside:
        a.output.mkdir(parents=True, exist_ok=True)
        repo = Path(__file__).resolve().parents[2]
        cmd = ['docker', 'run', '--rm', '--pull=never', '--platform', f'linux/{a.arch}',
               '-v', f'{repo}:/repo:ro', '-v', f'{a.sources.resolve()}:/sources:ro',
               '-v', f'{a.output.resolve()}:/out', IMAGE, 'sh', '-ec',
               'apk add --no-cache python3 cmake make g++ binutils file && exec python3 /repo/tools/compression-bench/build_linux.py --inside --stage "$1" --arch "$2"',
               'build-linux', a.stage, a.arch]
        subprocess.run(cmd, check=True)
        return
    root = Path('/out') / a.arch
    root.mkdir(exist_ok=True)
    manifest_path = root/'build.json'
    report = json.loads(manifest_path.read_text()) if manifest_path.exists() else {'image': IMAGE, 'arch': a.arch, 'commands': []}
    def run(cmd, **kwargs):
        cmd = list(map(str, cmd))
        report['commands'].append(cmd)
        result = subprocess.run(cmd, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, **kwargs)
        with (root/'build.log').open('a') as log:
            log.write('$ ' + repr(cmd) + '\n' + result.stdout + '\n')
        manifest_path.write_text(json.dumps(report, indent=2)+'\n')
        if result.returncode:
            raise RuntimeError(result.stdout[-8000:])
        return result.stdout.strip()
    report['rustc'] = run(['rustc', '-Vv'])
    if not report['rustc'].startswith('rustc 1.97.1 '):
        raise RuntimeError('Exact Rust 1.97.1 required')
    target = 'x86_64-unknown-linux-musl' if a.arch == 'amd64' else 'aarch64-unknown-linux-musl'
    if f'host: {target}' not in report['rustc']:
        raise RuntimeError('Unexpected container architecture')
    arch_flag = '-march=x86-64-v3' if a.arch == 'amd64' else '-march=armv8-a+crc'
    flags = f'-O3 -DNDEBUG {arch_flag} -ffunction-sections -fdata-sections'
    report.update(target=target, cflags=flags, compiler=run(['g++', '--version']), packages=run(['apk', 'info', '-v']))
    native = root/'native'
    native.mkdir(exist_ok=True)
    lib = native/'lib'
    lib.mkdir(exist_ok=True)
    spec = importlib.util.spec_from_file_location('prepare', '/repo/tools/compression-bench/prepare_native.py')
    prepare = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(prepare)
    report['sources'] = []
    for name, version, url, sha in prepare.SOURCES:
        archive = Path('/sources')/f'{name}-{version}.tar.gz'
        if hashlib.sha256(archive.read_bytes()).hexdigest() != sha:
            raise RuntimeError(f'Invalid archive {archive}')
        report['sources'].append(dict(name=name, version=version, url=url, sha256=sha))
        if not (native/f'{name}-{version}').exists():
            with tarfile.open(archive) as tf:
                tf.extractall(native, filter='data')
    snappy = native/'snappy-1.2.2'
    zstd = native/'zstd-1.5.7'
    lz4 = native/'lz4-1.10.0/lib'
    if a.stage == 'native':
        for build in ['snappy-build', 'zstd-build']:
            shutil.rmtree(native/build, ignore_errors=True)
        common = [f'-DCMAKE_C_FLAGS={arch_flag}', f'-DCMAKE_CXX_FLAGS={arch_flag}', '-DCMAKE_BUILD_TYPE=Release', f'-DCMAKE_C_FLAGS_RELEASE={flags}', f'-DCMAKE_CXX_FLAGS_RELEASE={flags}', '-DCMAKE_POLICY_VERSION_MINIMUM=3.5']
        run(['cmake', '-S', snappy, '-B', native/'snappy-build', *common, '-DSNAPPY_BUILD_TESTS=OFF', '-DSNAPPY_BUILD_BENCHMARKS=OFF', '-DBUILD_SHARED_LIBS=OFF'])
        run(['cmake', '--build', native/'snappy-build', '--parallel', '4'])
        run(['cmake', '-S', zstd/'build/cmake', '-B', native/'zstd-build', *common, '-DZSTD_BUILD_PROGRAMS=OFF', '-DZSTD_BUILD_TESTS=OFF', '-DZSTD_BUILD_SHARED=OFF', '-DZSTD_MULTITHREAD_SUPPORT=OFF'])
        run(['cmake', '--build', native/'zstd-build', '--target', 'libzstd_static', '--parallel', '4'])
        shutil.copy2(native/'snappy-build/libsnappy.a', lib)
        shutil.copy2(native/'zstd-build/lib/libzstd.a', lib)
        for source in ['lz4', 'lz4hc']:
            run(['gcc', *flags.split(), '-c', lz4/f'{source}.c', '-o', native/f'{source}.o'])
        run(['ar', 'rcs', lib/'liblz4.a', native/'lz4.o', native/'lz4hc.o'])
        run(['g++', *flags.split(), '-std=c++17', f'-I{snappy}', f'-I{native}/snappy-build', f'-I{zstd}/lib', f'-I{lz4}', '-c', '/repo/tools/compression-bench/native.cc', '-o', native/'bench_native.o'])
        run(['ar', 'rcs', lib/'libbench_native.a', native/'bench_native.o'])
        report['snappy_config'] = (native/'snappy-build/config.h').read_text()
        report['native_shim_sha256'] = hashlib.sha256(Path('/repo/tools/compression-bench/native.cc').read_bytes()).hexdigest()
        report['archives'] = {f.name: hashlib.sha256(f.read_bytes()).hexdigest() for f in lib.glob('*.a')}
        report['native_ready'] = True
    else:
        if not report.get('native_ready'):
            raise RuntimeError('Run native stage first')
        if report['native_shim_sha256'] != hashlib.sha256(Path('/repo/tools/compression-bench/native.cc').read_bytes()).hexdigest():
            raise RuntimeError('Native shim changed; rebuild native stage')
        checkout = root/'checkout'
        checkout.mkdir(exist_ok=True)
        for name in ['crates', 'internal', 'pty', 'travel-android', 'travel-apple', 'docs', 'assets', 'README.md', 'pty-apple', 'pty-android', 'openwrt']:
            link = checkout/name
            if not link.exists():
                link.symlink_to(Path('/repo')/name)
        project = checkout/'tools/compression-bench'
        shutil.copytree('/repo/tools/compression-bench', project, dirs_exist_ok=True, ignore=shutil.ignore_patterns('target', 'results', '__pycache__'))
        env = dict(os.environ, CARGO_HOME=str(root/'cargo'), CARGO_TARGET_DIR=str(root/'target'), BENCH_NATIVE_DIR=str(native),
                   RUSTFLAGS='-C target-feature=+crt-static -C link-arg=-static -C link-arg=-static-libstdc++ -C link-arg=-static-libgcc')
        report['rustflags'] = env['RUSTFLAGS']
        report['benchmark_sources'] = {str(f.relative_to(project)): hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(project.rglob('*')) if f.is_file() and (f.suffix in ['.rs', '.cc'] or f.name in ['Cargo.toml', 'Cargo.lock'])}
        run(['cargo', 'test', '--locked', '--release', '--manifest-path', project/'Cargo.toml', '--target', target], env=env)
        run(['cargo', 'build', '--locked', '--release', '--manifest-path', project/'Cargo.toml', '--target', target], env=env)
        binary = root/'flowsplice-compression-bench'
        shutil.copy2(root/'target'/target/'release/flowsplice-compression-bench', binary)
        report['file'] = run(['file', binary])
        report['elf_header'] = run(['readelf', '-h', binary])
        report['program_headers'] = run(['readelf', '-l', binary])
        report['dynamic_section'] = run(['readelf', '-d', binary])
        if 'INTERP' in report['program_headers'] or '(NEEDED)' in report['dynamic_section']:
            raise RuntimeError('Binary is not fully static')
        report['binary_sha256'] = hashlib.sha256(binary.read_bytes()).hexdigest()
        report['binary_bytes'] = binary.stat().st_size
        bundle = Path('/out/frozen-corpus.bin')
        if not bundle.is_file():
            raise RuntimeError('Root-provided /out/frozen-corpus.bin required for identical-corpus smoke')
        report['smoke_label'] = 'Correctness-only smoke; local container/emulator timings are not remote performance results'
        report['bundle_sha256'] = hashlib.sha256(bundle.read_bytes()).hexdigest()
        run([binary, '--bundle', bundle, root/'smoke', '1', '5'])
        smoke = json.loads((root/'smoke/results.json').read_text())
        report['smoke_peak_rss_kib'] = smoke['peak_rss_kib']
        report['smoke_cases'] = len(smoke['cases'])
        if len(smoke['cases']) != 200 or smoke['peak_rss_kib'] >= 256 * 1024:
            raise RuntimeError('Smoke case count/RSS failed')
        report['final_ready'] = True
    manifest_path.write_text(json.dumps(report, indent=2)+'\n')
    print(f'{a.arch} {a.stage}: PASS', flush=True)


if __name__ == '__main__':
    main()
