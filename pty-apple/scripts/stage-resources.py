#!/usr/bin/env python3
"""Copy shared UI; private inputs and every build staging path must be Git-free."""
import importlib.util
import os
from pathlib import Path
import shutil
import sys

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
    for name in ['deployment-root.pub', 'business.json']:
        policy.external_path(str(source / name), require_file=True, must_exist=True)
output.mkdir(parents=True, exist_ok=True)
ui = output / 'pty-ui'
if ui.exists():
    shutil.rmtree(ui)
shutil.copytree(repo / 'pty-web/dist', ui)
bootstrap = output / 'bootstrap'
if bootstrap.exists():
    shutil.rmtree(bootstrap)
if private:
    bootstrap.mkdir(mode=0o700)
    for name in ['deployment-root.pub', 'business.json']:
        shutil.copyfile(source / name, bootstrap / name)
        (bootstrap / name).chmod(0o600)
