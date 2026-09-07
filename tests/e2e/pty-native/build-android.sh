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
for name in deployment-root.pub business.json; do
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
  --descriptor "$fixture_dir/business.json" \
  --output "$output_dir" \
  --target aarch64-linux-android
export CARGO_TARGET_DIR="$output_dir/cargo-target"
export GRADLE_USER_HOME="$output_dir/gradle-home"
export npm_config_cache="$output_dir/npm-cache"
export FLOWSPLICE_PTY_DEBUG_KEYSTORE="$output_dir/debug.keystore"
cd -- "$output_dir/source/pty-android"
./gradlew --no-daemon --no-configuration-cache --no-build-cache \
  -PflowsplicePrivateBuild=true :app:assembleDebugAndroidTest
printf 'Application APK: %s/source/pty-android/app/build/outputs/apk/debug/app-debug.apk\n' "$output_dir"
printf 'Instrumentation APK: %s/source/pty-android/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk\n' "$output_dir"
