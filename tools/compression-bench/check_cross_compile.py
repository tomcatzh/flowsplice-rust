#!/usr/bin/env python3
"""Compile/link-only mobile feasibility check. Never installs tools or runs binaries."""
import argparse
import hashlib
import json
import os
import shutil
from pathlib import Path
import subprocess


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--link-only', action='store_true', help='Reuse completed native builds and repeat decoder-only link checks')
    p.add_argument('--sources', type=Path, default=Path('/private/tmp/flowsplice-compression-bench/native'))
    p.add_argument('--output', type=Path, default=Path('/private/tmp/flowsplice-compression-cross'))
    p.add_argument('--developer-dir', default='/Applications/Xcode.app/Contents/Developer')
    p.add_argument('--ndk', type=Path, default=Path.home() / 'Library/Android/sdk/ndk/29.0.14206865')
    a = p.parse_args()
    a.output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, DEVELOPER_DIR=a.developer_dir)
    commands = []
    def run(cmd):
        cmd = list(map(str, cmd))
        commands.append(cmd)
        result = subprocess.run(cmd, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        with (a.output / 'build.log').open('a') as log:
            log.write('$ ' + repr(cmd) + '\n' + result.stdout + result.stderr + '\n')
        if result.returncode:
            raise RuntimeError((result.stdout + result.stderr)[-6000:])
        return result.stdout.strip()
    installed = run(['rustup', 'target', 'list', '--installed']).splitlines()
    report = {'xcode': run(['xcodebuild', '-version']), 'sdks': run(['xcodebuild', '-showsdks']),
              'ndk': (a.ndk / 'source.properties').read_text(), 'rustc': run(['rustc', '-Vv']),
              'installed_rust_targets': installed, 'targets': [], 'commands': commands}
    rust = a.output / 'probe.rs'
    rust.write_text('''#![no_std]
use core::ffi::{c_char,c_int};
unsafe extern "C" {
 fn mobile_decode(kind:c_int,src:*const c_char,n:usize,dst:*mut c_char,cap:usize)->usize;
 fn mobile_versions()->*const c_char;
}
#[panic_handler] fn panic(_: &core::panic::PanicInfo)->! {loop {}}
#[unsafe(no_mangle)] pub unsafe extern "C" fn rust_probe()->i32 { unsafe {
 // Link-only input: this executable is never run; no compression API is referenced.
 let input=[0u8;8]; let mut output=[0u8;64]; let mut total=0usize;
 for kind in 1..=3 { total^=mobile_decode(kind,input.as_ptr().cast(),8,output.as_mut_ptr().cast(),64); }
 if mobile_versions().is_null() {return 1;} (total & 127) as i32
}}
''')
    decoder = a.output / 'decoder.cc'
    decoder.write_text('''#include <snappy.h>
#include <zstd.h>
#include <lz4.h>
#include <climits>
#include <limits>
extern "C" size_t mobile_decode(int kind,const char* src,size_t n,char* dst,size_t cap) {
 const size_t error=std::numeric_limits<size_t>::max();
 if (kind==1) { size_t size=ZSTD_decompress(dst,cap,src,n); return ZSTD_isError(size)?error:size; }
 if (kind==2) { size_t expected=0; if(!snappy::GetUncompressedLength(src,n,&expected)||expected>cap) return error; return snappy::RawUncompress(src,n,dst)?expected:error; }
 if (kind==3 && n<=INT_MAX && cap<=INT_MAX) { int size=LZ4_decompress_safe(src,dst,(int)n,(int)cap); return size<0?error:(size_t)size; }
 return error;
}
extern "C" const char* mobile_versions() { return "snappy=1.2.2;zstd=" ZSTD_VERSION_STRING ";lz4=" LZ4_VERSION_STRING; }
''')
    probe = a.output / 'main.cc'
    probe.write_text('extern "C" int rust_probe();\nint main() { return rust_probe(); }\n')
    for name, target, sdk in [('ios-arm64', 'aarch64-apple-ios', 'iphoneos'), ('ios-simulator-arm64', 'aarch64-apple-ios-sim', 'iphonesimulator'), ('android-arm64-v8a', 'aarch64-linux-android', None)]:
        root = a.output / name
        root.mkdir(exist_ok=True)
        # Feature probes must not survive a changed SDK/compiler configuration.
        for build in ['snappy', 'zstd']:
            if not a.link_only:
                shutil.rmtree(root / build, ignore_errors=True)
        entry = {'name': name, 'rust_target': target}
        report['targets'].append(entry)
        try:
            if sdk:
                sysroot = run(['xcrun', '--sdk', sdk, '--show-sdk-path'])
                cc = run(['xcrun', '--sdk', sdk, '--find', 'clang'])
                cxx = run(['xcrun', '--sdk', sdk, '--find', 'clang++'])
                ar = run(['xcrun', '--sdk', sdk, '--find', 'ar'])
                triple = 'arm64-apple-ios17.0' + ('-simulator' if sdk == 'iphonesimulator' else '')
                flags = ['-target', triple, '-isysroot', sysroot]
                cmake = ['-DCMAKE_SYSTEM_NAME=iOS', '-DCMAKE_OSX_ARCHITECTURES=arm64', '-DCMAKE_OSX_DEPLOYMENT_TARGET=17.0', f'-DCMAKE_OSX_SYSROOT={sysroot}']
                entry.update(sdk=sysroot, sdk_version=run(['xcrun', '--sdk', sdk, '--show-sdk-version']), deployment_target='17.0')
            else:
                tool = a.ndk / 'toolchains/llvm/prebuilt/darwin-x86_64/bin'
                cc, cxx, ar = tool/'aarch64-linux-android34-clang', tool/'aarch64-linux-android34-clang++', tool/'llvm-ar'
                flags = []
                cmake = [f'-DCMAKE_TOOLCHAIN_FILE={a.ndk}/build/cmake/android.toolchain.cmake', '-DANDROID_ABI=arm64-v8a', '-DANDROID_PLATFORM=android-34', '-DANDROID_STL=c++_static']
                entry.update(api=34, abi='arm64-v8a')
            entry['compiler'] = run([cxx, '--version'])
            common = [*cmake, '-DCMAKE_BUILD_TYPE=Release', '-DCMAKE_POSITION_INDEPENDENT_CODE=ON', '-DCMAKE_POLICY_VERSION_MINIMUM=3.5', f'-DCMAKE_C_COMPILER={cc}', f'-DCMAKE_CXX_COMPILER={cxx}']
            snappy = a.sources/'snappy-1.2.2'
            zstd = a.sources/'zstd-1.5.7'
            lz4 = a.sources/'lz4-1.10.0/lib'
            if not a.link_only:
                run(['cmake', '-S', snappy, '-B', root/'snappy', *common, '-DSNAPPY_BUILD_TESTS=OFF', '-DSNAPPY_BUILD_BENCHMARKS=OFF', '-DBUILD_SHARED_LIBS=OFF'])
                run(['cmake', '--build', root/'snappy', '--parallel', '4'])
                run(['cmake', '-S', zstd/'build/cmake', '-B', root/'zstd', *common, '-DZSTD_BUILD_PROGRAMS=OFF', '-DZSTD_BUILD_TESTS=OFF', '-DZSTD_BUILD_SHARED=OFF', '-DZSTD_MULTITHREAD_SUPPORT=OFF'])
                run(['cmake', '--build', root/'zstd', '--target', 'libzstd_static', '--parallel', '4'])
                for source in ['lz4', 'lz4hc']:
                    run([cc, *flags, '-O3', '-DNDEBUG', '-fPIC', '-c', lz4/f'{source}.c', '-o', root/f'{source}.o'])
                run([ar, 'rcs', root/'liblz4.a', root/'lz4.o', root/'lz4hc.o'])
                run([cxx, *flags, '-std=c++17', '-O3', '-DNDEBUG', '-fPIC', f'-I{snappy}', f'-I{root}/snappy', f'-I{zstd}/lib', f'-I{lz4}', '-c', Path(__file__).with_name('native.cc').resolve(), '-o', root/'native.o'])
                run([ar, 'rcs', root/'libbench_native.a', root/'native.o'])
            libs = [root/'libbench_native.a', root/'snappy/libsnappy.a', root/'zstd/lib/libzstd.a', root/'liblz4.a']
            entry['archives'] = {}
            for lib in libs:
                arch = run(['xcrun', 'lipo', '-archs', lib]) if sdk else run([tool/'llvm-readobj', '--file-headers', lib])
                entry['archives'][lib.name] = {'sha256': hashlib.sha256(lib.read_bytes()).hexdigest(), 'architecture': arch}
            if target not in installed:
                raise RuntimeError('Rust target unavailable; native libraries built but Rust link check skipped')
            run([cxx, *flags, '-std=c++17', '-O2', '-ffunction-sections', '-fdata-sections', f'-I{snappy}', f'-I{root}/snappy', f'-I{zstd}/lib', f'-I{lz4}', '-c', decoder, '-o', root/'decoder.o'])
            run(['rustc', '--edition=2024', '--crate-type=staticlib', '-C', 'panic=abort', '-C', 'opt-level=2', '--target', target, rust, '-o', root/'librust_probe.a'])
            run([cxx, *flags, '-O2', probe, root/'librust_probe.a', root/'decoder.o', *libs[1:], *(['-Wl,-dead_strip'] if sdk else ['-Wl,--gc-sections']), *([] if sdk else ['-static-libstdc++', '-ldl', '-lm']), '-o', root/'link-probe'])
            entry['decoder_undefined_symbols'] = run((['xcrun', 'nm', '-u'] if sdk else [tool/'llvm-nm', '-u']) + [root/'decoder.o'])
            entry['linked_binary'] = run(['file', root/'link-probe'])
            entry['load_commands'] = run(['xcrun', 'otool', '-l', root/'link-probe']) if sdk else run([tool/'llvm-readelf', '-h', '-d', root/'link-probe'])
            entry['status'] = 'compiled and linked; not executed'
            print(name + ': PASS', flush=True)
        except Exception as error:
            entry['status'] = 'failed'
            entry['error'] = str(error)
            print(name + ': FAIL: ' + str(error)[-1000:], flush=True)
        finally:
            (a.output/'cross-build.json').write_text(json.dumps(report, indent=2)+'\n')
    if any(t['status']=='failed' for t in report['targets']):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
