#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
android_root="${repo_root}/travel-android"
profile_zip="${1:?Android Travel profile ZIP is required}"
password_file="${2:?Android Travel profile password file is required}"
if [[ -z "${JAVA_HOME:-}" && -d "/Applications/Android Studio.app/Contents/jbr/Contents/Home" ]]; then
  export JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
fi

if [[ ! -s "${profile_zip}" || ! -s "${password_file}" ]]; then
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

(
  cd "${android_root}"
  ./gradlew --no-daemon :app:assembleDebug :app:assembleDebugAndroidTest
)

app_id='io.zxf.flowsplice.travel'
remote_profile='/data/local/tmp/flowsplice-android-travel-profile.zip'
"${adb}" install -r "${android_root}/app/build/outputs/apk/debug/flowsplice-travel.apk" >/dev/null
"${adb}" install -r "${android_root}/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk" >/dev/null
"${adb}" shell pm clear "${app_id}" >/dev/null
"${adb}" push "${profile_zip}" "${remote_profile}" >/dev/null
"${adb}" shell chmod 644 "${remote_profile}"
"${adb}" shell pm grant "${app_id}" android.permission.POST_NOTIFICATIONS

profile_password="$(tr -d '\r\n' <"${password_file}")"
instrumentation_output="$("${adb}" shell am instrument -w -r \
  -e class io.zxf.flowsplice.travel.TravelCoreDockerE2ETest \
  -e profile_path "${remote_profile}" \
  -e profile_password "${profile_password}" \
  "${app_id}.test/androidx.test.runner.AndroidJUnitRunner")"
printf '%s\n' "${instrumentation_output}"
if ! grep -Fq 'OK (1 test)' <<<"${instrumentation_output}"; then
  echo 'Android Travel instrumentation E2E did not pass' >&2
  exit 1
fi
"${adb}" shell rm -f "${remote_profile}"
printf '%s\n' '{"checkpoint":"android-native-core-docker-background-roundtrip"}'
