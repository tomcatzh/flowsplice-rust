#!/usr/bin/env bash
# Build private ARM64 application and instrumentation APKs; never operate a device.
set -euo pipefail
if [[ "$#" -ne 2 ]]; then
  printf 'Usage: %s FIXTURE_DIR OUTPUT_DIR\n' "$0" >&2
  exit 2
fi
fixture_dir="$1"
output_dir="$2"
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
for path in "$fixture_dir" "$output_dir"; do
  if [[ "$path" != /* ]]; then
    printf 'Fixture and output directories must be absolute.\n' >&2
    exit 2
  fi
  python3 "$repo_root/scripts/private-travel-trust.py" check-path --path "$path"
done
configuration=business.json
configuration_flag=--descriptor
if [[ -f "$fixture_dir/homes.json" ]]; then configuration=homes.json; configuration_flag=--homes; fi
for name in deployment-root.pub "$configuration"; do
  if [[ ! -f "$fixture_dir/$name" ]]; then
    printf 'Required private fixture input is missing: %s\n' "$name" >&2
    exit 2
  fi
  python3 "$repo_root/scripts/private-travel-trust.py" check-path --path "$fixture_dir/$name"
done
export JAVA_HOME="${JAVA_HOME:-/Applications/Android Studio.app/Contents/jbr/Contents/Home}"
export ANDROID_SDK_ROOT="${ANDROID_SDK_ROOT:-/Users/tomcat/Library/Android/sdk}"
python3 "$repo_root/pty-android/scripts/build-private.py" \
  --root "$fixture_dir/deployment-root.pub" \
  "$configuration_flag" "$fixture_dir/$configuration" \
  --output "$output_dir" \
  --target aarch64-linux-android
export CARGO_TARGET_DIR="$output_dir/cargo-target"
export GRADLE_USER_HOME="$output_dir/gradle-home"
export npm_config_cache="$output_dir/npm-cache"
export FLOWSPLICE_PTY_DEBUG_KEYSTORE="$output_dir/debug.keystore"
cd -- "$output_dir/source/pty-android"
./gradlew --no-daemon --no-configuration-cache --no-build-cache \
  -PflowsplicePrivateBuild=true :app:assembleDebugAndroidTest
python3 - "$fixture_dir" "$output_dir" "$configuration" <<'PY_VERIFY'
from pathlib import Path
import sys
import zipfile
fixture, output, configuration = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
with zipfile.ZipFile(output / 'source/pty-android/app/build/outputs/apk/debug/app-debug.apk') as apk:
    for name in ['deployment-root.pub', configuration]:
        if apk.read('assets/bootstrap/' + name) != (fixture / name).read_bytes():
            raise SystemExit('Bundled bootstrap differs from fixture')
    if any(name.startswith('assets/bootstrap/') and 'password' in name for name in apk.namelist()):
        raise SystemExit('Password fixture must never be bundled')
PY_VERIFY
printf 'Application APK: %s/source/pty-android/app/build/outputs/apk/debug/app-debug.apk\n' "$output_dir"
printf 'Instrumentation APK: %s/source/pty-android/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk\n' "$output_dir"
