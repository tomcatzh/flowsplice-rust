#!/usr/bin/env bash
# Build the Android Travel app and its instrumentation test APK from a staged copy of the source,
# with the given deployment root, into OUTPUT. Called by tests/e2e/android/run.sh through
# tests/e2e/native-build-cache.py, which skips it when nothing it is built from has changed.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
stage="${1:?stage directory is required}"
test_root="${2:?deployment root is required}"
output="${3:?output directory is required}"

python3 "${repo_root}/tests/e2e/stage-source.py" "${repo_root}" "${stage}/source"
cp -- "${test_root}" "${stage}/deployment-root.pub"
android_root="${stage}/source/travel-android"
python3 "${repo_root}/scripts/private-travel-trust.py" copy-root \
  --source "${stage}/deployment-root.pub" \
  --destination "${android_root}/app/src/debug/assets/bootstrap/deployment-root.pub"
mkdir -p "${stage}/tmp"
export CARGO_TARGET_DIR="${stage}/cargo-target"
# Gradle and its dependencies are public downloads, so on a Mac they stay in the E2E cache and a
# rebuild does not fetch them again. Private packaging keeps its own stage-owned Gradle home.
if [[ -n "${FLOWSPLICE_E2E_CACHE_DIR:-}" ]]; then
  export GRADLE_USER_HOME="${FLOWSPLICE_E2E_CACHE_DIR}/gradle-home"
else
  export GRADLE_USER_HOME="${stage}/gradle-home"
fi
export TMPDIR="${stage}/tmp"
(
  cd "${android_root}"
  # Retry when a download fails on the network, after a pause: network faults here tend to last a
  # minute or so. The next attempt keeps what was downloaded.
  for attempt in 1 2 3; do
    gradle_log="${stage}/gradle-attempt-${attempt}.log"
    if ./gradlew --no-daemon --no-build-cache --no-configuration-cache \
      :app:assembleDebug :app:assembleDebugAndroidTest 2>&1 | tee "${gradle_log}"; then
      exit 0
    fi
    if ! grep -Eiq 'could not (get|head|resolve)|premature eof|terminated the handshake|read timed out|connection reset' "${gradle_log}"; then
      exit 1
    fi
    if [[ "${attempt}" -lt 3 ]]; then
      echo "Gradle attempt ${attempt} failed on the network; retrying in $((attempt * 30)) s" >&2
      sleep "$((attempt * 30))"
    fi
  done
  exit 1
)
mkdir -p "${output}"
cp "${android_root}/app/build/outputs/apk/debug/flowsplice-travel.apk" \
  "${android_root}/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk" "${output}/"

