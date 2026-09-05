#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
android_root="${repo_root}/travel-android"
password_file="${1:?Android Travel private-key password file is required}"
relay_address="${2:-10.0.2.2:18446}"
if [[ -z "${JAVA_HOME:-}" && -d "/Applications/Android Studio.app/Contents/jbr/Contents/Home" ]]; then
  export JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
fi

if [[ ! -s "${password_file}" ]]; then
  echo 'Android Travel E2E inputs are missing' >&2
  exit 1
fi

sdk_root="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
if [[ -z "${sdk_root}" ]]; then
  sdk_root="$(sed -n 's/^sdk.dir=//p' "${android_root}/local.properties" | head -n 1)"
fi
adb="${sdk_root}/platform-tools/adb"
if [[ ! -x "${adb}" ]]; then
  echo 'Android Debug Bridge was not found in the configured SDK' >&2
  exit 1
fi
if [[ "$("${adb}" get-state 2>/dev/null)" != "device" ]]; then
  echo 'No ready Android emulator or device is connected' >&2
  exit 1
fi

stage="$(mktemp -d "${TMPDIR:-/private/tmp}/flowsplice-android-e2e.XXXXXX")"
instrumentation_output="${stage}/instrumentation.log"
instrumentation_pid=''
cleanup() {
  if [[ -n "${instrumentation_pid}" ]] && kill -0 "${instrumentation_pid}" 2>/dev/null; then
    kill "${instrumentation_pid}" 2>/dev/null || true
    wait "${instrumentation_pid}" 2>/dev/null || true
  fi
  rm -rf -- "${stage}"
}
trap cleanup EXIT

# Only the Docker E2E fixture has a repository-local default. Explicit inputs
# must be external, just like private release inputs.
test_root="${FLOWSPLICE_E2E_DEPLOYMENT_ROOT_FILE:-${repo_root}/tests/e2e/generated/certs/deployment-root.pub}"
if [[ ! -s "${test_root}" ]]; then
  echo 'Android Travel E2E deployment-root fixture is missing' >&2
  exit 1
fi
if [[ -n "${FLOWSPLICE_E2E_DEPLOYMENT_ROOT_FILE:-}" ]]; then
  python3 "${repo_root}/scripts/private-travel-trust.py" check-path --path "${test_root}"
fi
python3 "${repo_root}/tests/e2e/stage-source.py" "${repo_root}" "${stage}/source"
cp -- "${test_root}" "${stage}/deployment-root.pub"
android_root="${stage}/source/travel-android"
python3 "${repo_root}/scripts/private-travel-trust.py" copy-root \
  --source "${stage}/deployment-root.pub" \
  --destination "${android_root}/app/src/debug/assets/bootstrap/deployment-root.pub"
mkdir -p "${stage}/tmp"
export ANDROID_SDK_ROOT="${sdk_root}"
export CARGO_TARGET_DIR="${stage}/cargo-target"
export GRADLE_USER_HOME="${stage}/gradle-home"
export TMPDIR="${stage}/tmp"
(
  cd "${android_root}"
  ./gradlew --no-daemon --no-build-cache --no-configuration-cache \
    :app:assembleDebug :app:assembleDebugAndroidTest
)

app_id='io.zxf.flowsplice.travel'
"${adb}" install -r "${android_root}/app/build/outputs/apk/debug/flowsplice-travel.apk" >/dev/null
"${adb}" install -r "${android_root}/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk" >/dev/null
"${adb}" shell pm clear "${app_id}" >/dev/null
"${adb}" shell pm grant "${app_id}" android.permission.POST_NOTIFICATIONS

private_key_password="$(tr -d '\r\n' <"${password_file}")"
"${adb}" shell am instrument -w -r \
  -e class io.zxf.flowsplice.travel.TravelCoreDockerE2ETest \
  -e relay_address "${relay_address}" \
  -e private_key_password "${private_key_password}" \
  "${app_id}.test/androidx.test.runner.AndroidJUnitRunner" >"${instrumentation_output}" 2>&1 &
instrumentation_pid=$!

pending="$(python3 "${repo_root}/tests/e2e/home-issuer-client.py" pending \
  --port 19084 \
  --travel-id android-e2e-travel \
  --wait-secs 120)"
request_id="$(python3 -c \
  'import json,sys; print(json.loads(sys.argv[1])["request_id"])' \
  "${pending}")"
home_verification_code="$(python3 -c \
  'import json,sys; print(json.loads(sys.argv[1])["verification_code"])' \
  "${pending}")"
android_verification_code=''
for _ in $(seq 1 30); do
  android_verification_code="$(
    "${adb}" shell run-as "${app_id}" cat files/e2e-verification-code 2>/dev/null \
      | tr -d '\r\n' || true
  )"
  if [[ -n "${android_verification_code}" ]]; then
    break
  fi
  sleep 1
done
if [[ "${android_verification_code}" != "${home_verification_code}" ]]; then
  echo 'Android and Home verification codes did not match' >&2
  exit 1
fi
python3 "${repo_root}/tests/e2e/home-issuer-client.py" approve \
  --port 19084 \
  --request-id "${request_id}" \
  --password-file "${password_file}" \
  --scope global \
  --valid-days 1 >/dev/null

set +e
wait "${instrumentation_pid}"
instrumentation_status=$?
set -e
instrumentation_pid=''
sed -n '1,240p' "${instrumentation_output}"
if [[ "${instrumentation_status}" -ne 0 ]] || ! grep -Fq 'OK (1 test)' "${instrumentation_output}"; then
  echo 'Android Travel instrumentation E2E did not pass' >&2
  exit 1
fi
performance="$("${adb}" shell run-as "${app_id}" cat files/e2e-performance.json | tr -d '\r\n')"
if [[ -z "${performance}" ]]; then
  echo 'Android performance evidence was not produced' >&2
  exit 1
fi
printf '%s\n' "${performance}"
printf '%s\n' '{"checkpoint":"android-network-switch-short-outage-idle-restart-mapping-roundtrip-performance"}'
