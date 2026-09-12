#!/usr/bin/env python3
"""Copy shared UI; private inputs and every build staging path must be Git-free."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

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

project = Path(os.environ['SRCROOT']).resolve()
repo = project.parent
output = Path(os.environ['TARGET_BUILD_DIR']) / os.environ['UNLOCALIZED_RESOURCES_FOLDER_PATH']
private = os.environ.get('FLOWSPLICE_PTY_BOOTSTRAP_DIR', '')
if private:
    spec = importlib.util.spec_from_file_location('private_trust', repo / 'scripts/private-travel-trust.py')
    policy = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = policy
    spec.loader.exec_module(policy)
    # This explicitly rejects a private build inside any Git checkout, including worktrees.
    for path in [repo, output, Path(os.environ['BUILT_PRODUCTS_DIR']), Path(os.environ['CARGO_TARGET_DIR']), Path(private)]:
        policy.external_path(str(path))
    for key in ["OBJROOT", "SYMROOT", "PROJECT_TEMP_DIR", "SHARED_PRECOMPS_DIR"]:
        if os.environ.get(key):
            policy.external_path(os.environ[key])
    source = policy.external_path(private, require_directory=True, must_exist=True)
    configurations = [name for name in ['service-class.json', 'homes.json', 'business.json'] if (source / name).is_file()]
    if len(configurations) != 1: raise ValueError('Private bootstrap requires exactly one service class, Home catalog or business descriptor')
    names = ['deployment-root.pub', configurations[0]]
    if any(not item.is_file() or item.name not in names for item in source.iterdir()): raise ValueError('Unexpected private bootstrap input')
    for name in names:
        policy.external_path(str(source / name), require_file=True, must_exist=True)
    if 'homes.json' in names: validate_homes(source / 'homes.json')
# Never trust ignored dist from an earlier checkout/build. Freeze dependencies and
# remove old output before building, so a failed build cannot be packaged.
web = repo / 'pty-web'
subprocess.run(['npm', 'ci'], cwd=web, check=True)
if (web / 'dist').exists():
    shutil.rmtree(web / 'dist')
subprocess.run(['npm', 'run', 'build'], cwd=web, check=True)
if not (web / 'dist/index.html').is_file():
    raise ValueError('PTY UI build did not produce index.html')
output.mkdir(parents=True, exist_ok=True)
ui = output / 'pty-ui'
if ui.exists():
    shutil.rmtree(ui)
shutil.copytree(repo / 'pty-web/dist', ui)
notices = output / 'ThirdPartyNotices'
notices.mkdir(exist_ok=True)
shutil.copyfile(repo / 'pty/codec/vendor/snappy/COPYING', notices / 'Snappy.txt')
bootstrap = output / 'bootstrap'
if bootstrap.exists():
    shutil.rmtree(bootstrap)
if private:
    bootstrap.mkdir(mode=0o700)
    for name in names:
        shutil.copyfile(source / name, bootstrap / name)
        (bootstrap / name).chmod(0o600)
