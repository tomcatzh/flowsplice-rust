#!/usr/bin/env bash
# Build private Apple test bundles only; never boot, install, or launch a simulator/app.
set -euo pipefail
umask 077
if [[ $# != 2 ]]; then
  echo 'Usage: build-apple.sh FIXTURE_DIR OUTPUT_DIR' >&2
  exit 2
fi
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
fixture_dir="$1"
output_dir="$2"
python3 - "${repo_root}" "${fixture_dir}" "${output_dir}" <<'PY'
import importlib.util
from pathlib import Path
import sys
repo, fixture, output = map(Path, sys.argv[1:])
if not fixture.is_absolute() or not output.is_absolute():
    raise SystemExit('Fixture and output directories must be absolute')
spec = importlib.util.spec_from_file_location('private_trust', repo / 'scripts/private-travel-trust.py')
policy = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = policy
spec.loader.exec_module(policy)
fixture = policy.external_path(str(fixture), must_exist=True, require_directory=True)
output = policy.external_path(str(output))
for name in ['deployment-root.pub', 'business.json', 'password.txt']:
    policy.external_path(str(fixture / name), must_exist=True, require_file=True)
if output.exists():
    raise SystemExit('Output directory must not already exist; choose a fresh build directory')
if fixture.is_relative_to(output) or output.is_relative_to(fixture):
    raise SystemExit('Fixture and output directories must be separate')
output.mkdir(mode=0o700, parents=True)
PY
# Canonicalize only after checking both lexical and resolved paths with the private policy.
fixture_dir="$(cd -- "${fixture_dir}" && pwd -P)"
output_dir="$(cd -- "${output_dir}" && pwd -P)"
python3 "${repo_root}/tests/e2e/stage-source.py" "${repo_root}" "${output_dir}/source" \
  >"${output_dir}/stage-source.log" 2>&1
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
export CARGO_TARGET_DIR="${output_dir}/cargo-target"
export FLOWSPLICE_PTY_BOOTSTRAP_DIR="${fixture_dir}"
export npm_config_cache="${output_dir}/npm-cache"
(
  cd "${output_dir}/source/pty-web"
  npm ci
  npm run build
) >"${output_dir}/pty-web-build.log" 2>&1
project="${output_dir}/source/pty-apple/FlowSplicePTY.xcodeproj"
xcodebuild -project "${project}" -scheme FlowSplicePTY-iOSUITests \
  -configuration Debug -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath "${output_dir}/ios-derived-data" ARCHS=arm64 ONLY_ACTIVE_ARCH=YES \
  CODE_SIGNING_ALLOWED=YES CODE_SIGN_IDENTITY=- build-for-testing >"${output_dir}/ios-build.log" 2>&1
macos_signing_args=("CODE_SIGN_IDENTITY=${FLOWSPLICE_PTY_MACOS_SIGNING_IDENTITY:--}")
if [[ -n "${FLOWSPLICE_PTY_MACOS_DEVELOPMENT_TEAM:-}" ]]; then
  macos_signing_args+=("DEVELOPMENT_TEAM=${FLOWSPLICE_PTY_MACOS_DEVELOPMENT_TEAM}")
fi
xcodebuild -project "${project}" -scheme FlowSplicePTY-macOSUITests \
  -configuration Debug -sdk macosx -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath "${output_dir}/macos-derived-data" ARCHS=arm64 ONLY_ACTIVE_ARCH=YES \
  CODE_SIGNING_ALLOWED=YES "${macos_signing_args[@]}" build-for-testing >"${output_dir}/macos-build.log" 2>&1
python3 - "${fixture_dir}" "${output_dir}" <<'PY'
import json
import plistlib
from pathlib import Path
import sys
fixture, output = map(Path, sys.argv[1:])
manifest = {'version': 1, 'platforms': {}}
for platform, configuration, identifier in [
    ('ios', 'Debug-iphonesimulator', 'io.zxf.flowsplice.pty'),
    ('macos', 'Debug', 'io.zxf.flowsplice.pty.macos'),
]:
    products = output / f'{platform}-derived-data/Build/Products'
    app = products / configuration / 'FlowSplicePTY.app'
    resources = app if platform == 'ios' else app / 'Contents/Resources'
    info_file = app / ('Info.plist' if platform == 'ios' else 'Contents/Info.plist')
    if not info_file.is_file():
        raise SystemExit(f'{platform} app bundle is missing')
    with info_file.open('rb') as stream:
        info = plistlib.load(stream)
    if info.get('CFBundleIdentifier') != identifier:
        raise SystemExit(f'{platform} application identity mismatch')
    for name in ['deployment-root.pub', 'business.json']:
        bundled = resources / 'bootstrap' / name
        if not bundled.is_file() or bundled.read_bytes() != (fixture / name).read_bytes():
            raise SystemExit(f'{platform} bundled private resource mismatch')
    if (resources / 'bootstrap/password.txt').exists():
        raise SystemExit('Password fixture must never be bundled')
    if not (resources / 'pty-ui/index.html').is_file():
        raise SystemExit(f'{platform} shared UI missing')
    runs = sorted(products.glob('*.xctestrun'))
    if len(runs) != 1:
        raise SystemExit(f'{platform} requires exactly one generated xctestrun')
    manifest['platforms'][platform] = {'app': str(app), 'xctestrun': str(runs[0])}
(output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
PY
printf 'Apple PTY test builds verified. Manifest: %s/manifest.json\n' "${output_dir}"
