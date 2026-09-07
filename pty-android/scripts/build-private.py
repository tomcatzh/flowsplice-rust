#!/usr/bin/env python3
"""Export and build a private Android package without staging private inputs in Git."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument('--root', required=True, type=Path)
parser.add_argument('--descriptor', required=True, type=Path)
parser.add_argument('--output', required=True, type=Path)
parser.add_argument('--target', required=True, choices=['aarch64-linux-android', 'x86_64-linux-android'])
parser.add_argument('--release', action='store_true')
args = parser.parse_args()
repo = Path(__file__).resolve().parents[2]
helper = repo / 'scripts/private-travel-trust.py'
def outside(path):
    if not path.is_absolute():
        raise SystemExit('Private paths must be absolute')
    subprocess.run(['python3', str(helper), 'check-path', '--path', str(path)], check=True, stdout=subprocess.DEVNULL)
for path in [args.root, args.descriptor, args.output]:
    outside(path)
if not args.root.is_file() or not args.descriptor.is_file():
    raise SystemExit('Both private bootstrap files are required')
json.loads(args.descriptor.read_text())
if args.release:
    for name in ['FLOWSPLICE_PTY_KEYSTORE', 'FLOWSPLICE_PTY_KEY_ALIAS', 'FLOWSPLICE_PTY_STORE_PASSWORD', 'FLOWSPLICE_PTY_KEY_PASSWORD']:
        if not os.environ.get(name): raise SystemExit('Release requires external signing environment')
    outside(Path(os.environ['FLOWSPLICE_PTY_KEYSTORE']))
args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
stage = args.output / 'source'
# Export only current tracked/unignored source files, never ignored fixture certificates/caches.
source_paths = subprocess.check_output(['git', '-C', str(repo), 'ls-files', '-z', '--cached', '--others', '--exclude-standard']).split(b'\0')
stage.mkdir()
for encoded in set(source_paths):
    if not encoded: continue
    relative = Path(os.fsdecode(encoded))
    if relative.parts[0] in {'.llmwiki', 'AGENTS.md', 'CLAUDE.md'}: continue
    source = repo / relative
    if source.is_symlink(): raise SystemExit('Source export rejects symlinks')
    if not source.is_file(): continue
    destination = stage / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)
env = os.environ.copy()
env.update(CARGO_TARGET_DIR=str(args.output / 'cargo-target'), GRADLE_USER_HOME=str(args.output / 'gradle-home'), npm_config_cache=str(args.output / 'npm-cache'))
subprocess.run(['npm', 'ci', '--prefix', str(stage / 'pty-web')], env=env, check=True)
subprocess.run(['npm', 'run', 'build', '--prefix', str(stage / 'pty-web')], env=env, check=True)
assets = stage / 'pty-android/app/src/main/assets'
(assets / 'bootstrap').mkdir(parents=True)
subprocess.run(['python3',str(helper),'copy-root','--source',str(args.root),'--destination',str(assets / 'bootstrap/deployment-root.pub')], check=True)
shutil.copyfile(args.descriptor, assets / 'bootstrap/business.json')
shutil.copytree(stage / 'pty-web/dist', assets / 'pty')
subprocess.run(['bash', str(stage / 'pty-android/scripts/build-native.sh'), args.target], cwd=stage, env=env, check=True)
if not args.release:
    debug_key = args.output / 'debug.keystore'
    keytool = Path(env.get('JAVA_HOME', '')) / 'bin/keytool'
    subprocess.run([str(keytool), '-genkeypair', '-keystore', str(debug_key), '-alias', 'androiddebugkey', '-storepass', 'android', '-keypass', 'android', '-dname', 'CN=Android Debug,O=Android,C=US', '-keyalg', 'RSA', '-validity', '10000'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    env['FLOWSPLICE_PTY_DEBUG_KEYSTORE'] = str(debug_key)
subprocess.run([str(stage / 'pty-android/gradlew'), '--no-daemon', '--no-configuration-cache', '--no-build-cache', '-PflowsplicePrivateBuild=true', ':app:assembleRelease' if args.release else ':app:assembleDebug'], cwd=stage / 'pty-android', env=env, check=True)
print('Private APK built in external source/pty-android/app/build/outputs/apk')
