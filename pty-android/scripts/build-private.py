#!/usr/bin/env python3
"""Export and build a private Android package without staging private inputs in Git."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess

def validate_homes(path):
    import re
    catalog = json.loads(path.read_text())
    homes = catalog.get('homes')
    if type(catalog.get('version')) is not int or catalog['version'] != 1 or not isinstance(homes, list) or not 1 <= len(homes) <= 8:
        raise ValueError('Invalid Home catalog')
    ids = set()
    for home in homes:
        identifier = home.get('id', '')
        name = home.get('name', '')
        relay = home.get('relay', '')
        if not isinstance(identifier, str) or not re.fullmatch(r'[a-z0-9][a-z0-9_-]{0,47}', identifier) or identifier in ids:
            raise ValueError('Invalid or duplicate Home id')
        ids.add(identifier)
        if not isinstance(name, str) or not name.strip() or len(name.encode()) > 128 or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in name):
            raise ValueError('Invalid Home name')
        if home.get('platform') not in ('linux', 'macos') or not isinstance(home.get('descriptor'), dict) or not isinstance(relay, str) or len(relay.encode()) > 512:
            raise ValueError('Invalid Home configuration')

parser = argparse.ArgumentParser()
parser.add_argument('--root', required=True, type=Path)
inputs = parser.add_mutually_exclusive_group(required=True)
inputs.add_argument('--descriptor', type=Path)
inputs.add_argument('--homes', type=Path)
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
configuration = args.homes or args.descriptor
for path in [args.root, configuration, args.output]:
    outside(path)
if not args.root.is_file() or not configuration.is_file():
    raise SystemExit('Both private bootstrap files are required')
if args.homes: validate_homes(args.homes)
else: json.loads(args.descriptor.read_text())
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
shutil.copyfile(configuration, assets / 'bootstrap' / ('homes.json' if args.homes else 'business.json'))
shutil.copytree(stage / 'pty-web/dist', assets / 'pty')
subprocess.run(['bash', str(stage / 'pty-android/scripts/build-native.sh'), args.target], cwd=stage, env=env, check=True)
if not args.release:
    debug_key = args.output / 'debug.keystore'
    keytool = Path(env.get('JAVA_HOME', '')) / 'bin/keytool'
    subprocess.run([str(keytool), '-genkeypair', '-keystore', str(debug_key), '-alias', 'androiddebugkey', '-storepass', 'android', '-keypass', 'android', '-dname', 'CN=Android Debug,O=Android,C=US', '-keyalg', 'RSA', '-validity', '10000'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    env['FLOWSPLICE_PTY_DEBUG_KEYSTORE'] = str(debug_key)
subprocess.run([str(stage / 'pty-android/gradlew'), '--no-daemon', '--no-configuration-cache', '--no-build-cache', '-PflowsplicePrivateBuild=true', ':app:assembleRelease' if args.release else ':app:assembleDebug'], cwd=stage / 'pty-android', env=env, check=True)
print('Private APK built in external source/pty-android/app/build/outputs/apk')
